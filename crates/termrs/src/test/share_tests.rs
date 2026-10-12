    use super::*;

    #[test]
    fn codes_are_random_and_unambiguous() {
        let a = generate_code(6);
        let b = generate_code(6);
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(!a.contains(['0', 'O', '1', 'I']), "code: {a}");
        // Astronomically unlikely to collide; guards a broken generator.
        assert_ne!(a, b);
        assert_eq!(generate_code(1).len(), 4, "clamped to minimum 4");
    }

    fn shared_with(allow_control: bool) -> ShareShared {
        let (query_tx, _rx) = crossbeam_channel::unbounded();
        let (out_tx, _keep) = tokio::sync::broadcast::channel(4);
        ShareShared {
            pane_id: 1,
            code: "CODE".into(),
            allow_control,
            out_tx,
            query_tx,
            controller: Mutex::new(None),
        }
    }

    #[test]
    fn controller_is_exclusive_and_releasable() {
        let shared = shared_with(true);
        let a = iroh::SecretKey::generate().public();
        let b = iroh::SecretKey::generate().public();
        assert!(shared.claim_controller(&a), "first viewer controls");
        assert!(shared.claim_controller(&a), "same viewer keeps control");
        assert!(!shared.claim_controller(&b), "second viewer is read-only");
        shared.release_controller(&b);
        assert!(shared.claim_controller(&a), "wrong release is ignored");
        shared.release_controller(&a);
        assert!(shared.claim_controller(&b), "control transfers after release");
    }

    #[test]
    fn control_can_be_disabled() {
        let shared = shared_with(false);
        let a = iroh::SecretKey::generate().public();
        assert!(!shared.claim_controller(&a), "control disabled for everyone");
    }

    #[test]
    fn links_carry_ticket_and_code() {
        assert_eq!(
            build_link("https://x/#", "TICKET", "AB12"),
            "https://x/##ticket=TICKET&code=AB12"
        );
        assert_eq!(build_link("", "TICKET", "AB12"), "TICKET");
    }

    /// Real iroh round-trip: a client dials the ticket, gets the snapshot and
    /// its input reaches the host. Needs network (n0 relays); run with
    /// `cargo test -p termrs end_to_end_share -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires network (n0 relays)"]
    fn end_to_end_share() {
        use std::str::FromStr;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use termrs_share_proto::Hello;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        // Stand-in UI thread: answers snapshot/input queries.
        let (query_tx, query_rx) = crossbeam_channel::unbounded::<ShareQuery>();
        let got_input = Arc::new(AtomicBool::new(false));
        let got_input2 = got_input.clone();
        let responder = std::thread::spawn(move || {
            while let Ok(q) = query_rx.recv() {
                let reply = match q.kind {
                    ShareQueryKind::Snapshot { .. } => ShareReply::Bytes(b"SNAP".to_vec()),
                    ShareQueryKind::Input(b) => {
                        if b == b"typing" {
                            got_input2.store(true, Ordering::SeqCst);
                        }
                        ShareReply::Ok
                    }
                };
                let _ = q.reply.send(reply);
            }
        });

        let session = rt.block_on(async {
            let session = start(rt.handle(), query_tx, 1, "CODE".into(), "https://page/", true)
                .expect("share starts");

            // The ticket must carry a relay address, or browsers (which are
            // relay-only) cannot dial the endpoint.
            let ticket = EndpointTicket::from_str(&session.ticket).expect("ticket parses");
            assert!(
                ticket.endpoint_addr().addrs.iter().any(|a| a.is_relay()),
                "ticket has no relay address: {ticket:?}"
            );

            // Dial as a viewer.
            let client = Endpoint::builder(presets::N0)
                .bind()
                .await
                .expect("client binds");
            client.online().await;
            let conn = client
                .connect(ticket.endpoint_addr().clone(), ALPN)
                .await
                .expect("connects to host");
            let (mut send, mut recv) = conn.open_bi().await.expect("opens bi stream");
            write_frame(
                &mut send,
                &Frame::Hello(Hello {
                    mode: Mode::Control,
                    cols: 80,
                    rows: 24,
                    code: "CODE".into(),
                }),
            )
            .await
            .expect("sends hello");

            match read_frame(&mut recv).await.expect("reads frame") {
                Frame::Snapshot(b) => assert_eq!(b, b"SNAP"),
                other => panic!("expected snapshot, got {other:?}"),
            }

            write_frame(&mut send, &Frame::Input(b"typing".to_vec()))
                .await
                .expect("sends input");
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(
                got_input.load(Ordering::SeqCst),
                "host did not receive the viewer's input"
            );

            conn.close(0u8.into(), b"done");
            session
        });

        drop(session);
        drop(responder);
        rt.shutdown_background();
    }

    /// Diagnostic: dial a live share ticket and report what the host sends.
    /// Run with:
    /// `TERMRS_TICKET=<ticket> TERMRS_CODE=<code> cargo test -p termrs dial_shared_ticket -- --ignored --nocapture`
    #[test]
    #[ignore = "diagnostic; needs a live host and its ticket"]
    fn dial_shared_ticket() {
        use std::str::FromStr;
        use termrs_share_proto::Hello;

        let ticket_str = std::env::var("TERMRS_TICKET").expect("set TERMRS_TICKET");
        let code = std::env::var("TERMRS_CODE").unwrap_or_default();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ticket = match EndpointTicket::from_str(&ticket_str) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("TICKET PARSE FAILED: {e}");
                    return;
                }
            };
            eprintln!("endpoint id: {}", ticket.endpoint_addr().id);
            for a in &ticket.endpoint_addr().addrs {
                eprintln!("addr: {a:?}  relay={}", a.is_relay());
            }
            let client = match Endpoint::builder(presets::N0).bind().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("CLIENT BIND FAILED: {e}");
                    return;
                }
            };
            client.online().await;
            eprintln!("client online; dialing host…");
            let conn = match client
                .connect(ticket.endpoint_addr().clone(), ALPN)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("CONNECT FAILED: {e}");
                    return;
                }
            };
            eprintln!("CONNECTED");
            let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
            write_frame(
                &mut send,
                &Frame::Hello(Hello {
                    mode: Mode::Control,
                    cols: 80,
                    rows: 24,
                    code,
                }),
            )
            .await
            .expect("hello");
            for _ in 0..3 {
                match read_frame(&mut recv).await {
                    Ok(Frame::Snapshot(b)) => {
                        eprintln!("SNAPSHOT {} bytes: {:?}", b.len(), &b[..b.len().min(80)]);
                    }
                    Ok(Frame::Output(b)) => eprintln!("OUTPUT {} bytes", b.len()),
                    Ok(Frame::Error(e)) => eprintln!("HOST ERROR: {e}"),
                    Ok(other) => eprintln!("FRAME: {other:?}"),
                    Err(e) => {
                        eprintln!("READ ENDED: {e}");
                        break;
                    }
                }
            }
            conn.close(0u8.into(), b"done");
        });
        rt.shutdown_background();
    }
