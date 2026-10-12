    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 200, 60)
    }

    /// Pixel aspect of the fitted inner rect, for assertions.
    fn aspect(rect: Rect, cell_w: f32, cell_h: f32) -> f32 {
        let inner = Block::default().borders(Borders::ALL).inner(rect);
        (inner.width as f32 * cell_w) / (inner.height as f32 * cell_h)
    }

    /// Inner (texture) cells of a popup rect.
    fn inner_of(rect: Rect) -> Rect {
        Block::default().borders(Borders::ALL).inner(rect)
    }

    /// A 16:9 image keeps ~16:9 in pixels and stays inside the max box.
    #[test]
    fn fitted_popup_keeps_wide_aspect() {
        let max = inner_of(image_popup(area()));
        let r = image_popup_fitted(area(), (1600, 900), 10.0, 20.0);
        let a = aspect(r, 10.0, 20.0);
        assert!(
            (a - 16.0 / 9.0).abs() < 0.08,
            "aspect {a} for {r:?}"
        );
        let inner = inner_of(r);
        assert!(inner.x >= max.x && inner.y >= max.y, "inner {inner:?}");
        assert!(inner.x + inner.width <= max.x + max.width);
        assert!(inner.y + inner.height <= max.y + max.height);
        // Width-constrained: fills the box width.
        assert_eq!(inner.width, max.width);
    }

    /// A tall image is height-constrained and centered, not stretched.
    #[test]
    fn fitted_popup_keeps_tall_aspect() {
        let max = inner_of(image_popup(area()));
        let r = image_popup_fitted(area(), (400, 1200), 10.0, 20.0);
        let a = aspect(r, 10.0, 20.0);
        assert!((a - (400.0 / 1200.0)).abs() < 0.08, "aspect {a}");
        let inner = inner_of(r);
        assert_eq!(inner.height, max.height);
        assert!(inner.width < max.width, "inner {inner:?}");
        // Roughly centered horizontally inside the max box.
        let left = inner.x - max.x;
        let right = (max.x + max.width) - (inner.x + inner.width);
        assert!((left as i32 - right as i32).abs() <= 1, "l={left} r={right}");
    }

    /// Degenerate inputs fall back to the plain max box.
    #[test]
    fn fitted_popup_falls_back_when_degenerate() {
        assert_eq!(image_popup_fitted(area(), (0, 0), 10.0, 20.0), image_popup(area()));
        assert_eq!(image_popup_fitted(area(), (100, 100), 0.0, 20.0), image_popup(area()));
        assert_eq!(image_popup_fitted(Rect::new(0, 0, 3, 3), (100, 100), 10.0, 20.0), image_popup(Rect::new(0, 0, 3, 3)));
    }

    fn test_theme() -> crate::config::ThemeColors {
        crate::config::ThemeColors {
            fg: [220, 220, 220],
            bg: [10, 10, 10],
            cursor: None,
            cursor_shape: crate::config::CursorShape::Block,
            accent: None,
        }
    }

    /// A full block becomes a space painted in the glyph color, so the
    /// backend draws a seamless background quad instead of a font glyph.
    #[test]
    fn full_block_becomes_solid_fill() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf[(0, 0)].set_symbol("█").set_fg(Color::Green).set_bg(Color::Black);
        buf[(1, 0)].set_symbol("▀").set_fg(Color::Green).set_bg(Color::Black);
        buf[(2, 0)].set_symbol("a").set_fg(Color::Green).set_bg(Color::Black);
        buf[(3, 0)].set_symbol(" ").set_fg(Color::Green).set_bg(Color::Black);
        normalize_block_cells(&mut buf, Rect::new(0, 0, 4, 2), test_theme());
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].fg, Color::Green);
        assert_eq!(buf[(0, 0)].bg, Color::Green);
        // Halves, text and spaces are untouched.
        assert_eq!(buf[(1, 0)].symbol(), "▀");
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), " ");
    }

    /// `REVERSED` is baked into the fill (the backend must not swap again).
    #[test]
    fn full_block_reversed_uses_bg() {
        use ratatui::buffer::Buffer;
        use ratatui::style::Modifier;
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf[(0, 0)]
            .set_symbol("█")
            .set_fg(Color::Red)
            .set_bg(Color::Blue);
        buf[(0, 0)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        normalize_block_cells(&mut buf, Rect::new(0, 0, 2, 1), test_theme());
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].bg, Color::Blue);
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// `Reset` resolves against the theme, like the backend does.
    #[test]
    fn full_block_reset_resolves_theme() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf[(0, 0)]
            .set_symbol("█")
            .set_fg(Color::Reset)
            .set_bg(Color::Reset);
        normalize_block_cells(&mut buf, Rect::new(0, 0, 2, 1), test_theme());
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(220, 220, 220));
    }

    /// The Unicode braille bit layout maps to the 2×4 dot grid.
    #[test]
    fn braille_bits_map_to_grid() {
        assert_eq!(braille_dot_cell(0), (0, 0));
        assert_eq!(braille_dot_cell(1), (0, 1));
        assert_eq!(braille_dot_cell(2), (0, 2));
        assert_eq!(braille_dot_cell(3), (1, 0));
        assert_eq!(braille_dot_cell(4), (1, 1));
        assert_eq!(braille_dot_cell(5), (1, 2));
        assert_eq!(braille_dot_cell(6), (0, 3));
        assert_eq!(braille_dot_cell(7), (1, 3));
    }

    /// A braille cell is blanked and becomes one quad per set dot, colored
    /// with the cell foreground; a blank braille and normal text are untouched.
    #[test]
    fn braille_cells_become_dots() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        buf[(0, 0)]
            .set_symbol("\u{28FF}")
            .set_fg(Color::Green)
            .set_bg(Color::Black);
        buf[(1, 0)]
            .set_symbol("\u{2800}")
            .set_fg(Color::White)
            .set_bg(Color::Black);
        buf[(2, 0)]
            .set_symbol("a")
            .set_fg(Color::White)
            .set_bg(Color::Black);
        let out: Arc<Mutex<Vec<Dot>>> = Arc::new(Mutex::new(Vec::new()));
        collect_braille(
            &mut buf,
            Rect::new(0, 0, 4, 1),
            Rect::new(0, 0, 4, 1),
            &out,
            test_theme().fg,
        );
        assert_eq!(buf[(0, 0)].symbol(), " ", "braille glyph blanked");
        assert_eq!(buf[(1, 0)].symbol(), " ", "blank braille blanked");
        assert_eq!(buf[(2, 0)].symbol(), "a", "text untouched");
        let dots = out.lock().unwrap();
        assert_eq!(dots.len(), 8, "U+28FF has all eight dots");
        for d in dots.iter() {
            assert!(d.ndc[0] < d.ndc[2], "positive width");
            assert!(d.ndc[1] > d.ndc[3], "positive height");
            assert_eq!(d.color, srgb_to_linear([0, 205, 0]));
        }
    }

    /// Outside the pane's inner rect nothing is collected.
    #[test]
    fn braille_ignores_outside_inner() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        buf[(3, 0)].set_symbol("\u{28FF}").set_fg(Color::Green);
        let out: Arc<Mutex<Vec<Dot>>> = Arc::new(Mutex::new(Vec::new()));
        collect_braille(
            &mut buf,
            Rect::new(0, 0, 3, 1),
            Rect::new(0, 0, 4, 1),
            &out,
            test_theme().fg,
        );
        assert_eq!(out.lock().unwrap().len(), 0);
        assert_eq!(buf[(3, 0)].symbol(), "\u{28FF}");
    }
