// termrs shared-terminal viewer: xterm.js <-> wasm iroh client.

const $ = (id) => document.getElementById(id);

const statusEl = $("status");
const dialog = $("join");
const ticketInput = $("ticket");
const codeInput = $("code");

// A share link looks like `https://page/#ticket=<ticket>&code=<code>`.
const params = new URLSearchParams(location.hash.replace(/^#/, ""));

const term = new window.Terminal({
  cursorBlink: true,
  scrollback: 5000,
  fontSize: 14,
  fontFamily: "Consolas, 'Cascadia Mono', 'DejaVu Sans Mono', monospace",
  theme: { background: "#101014", foreground: "#dcdcdc" },
});
term.open($("terminal"));

let client = null;

term.onData((data) => {
  if (client) client.send(new TextEncoder().encode(data));
});

function setStatus(text) {
  statusEl.textContent = text;
}

function showJoin() {
  if (!dialog.open) dialog.showModal();
}

// Virtual special-key bar (phones have no Ctrl/Tab/Esc/arrows).
// Buttons send the same bytes a physical key would, and stay disabled
// until a session is connected.
const keysBar = $("keys");
function sendRaw(seq) {
  if (!client) return;
  client.send(new TextEncoder().encode(seq));
  term.focus();
}
function setKeysEnabled(on) {
  for (const b of keysBar.children) b.disabled = !on;
}
for (const [label, seq] of [
  ["Esc", "\x1b"],
  ["Tab", "\x09"],
  ["Enter", "\r"],
  ["Bksp", "\x7f"],
  ["↑", "\x1b[A"],
  ["↓", "\x1b[B"],
  ["←", "\x1b[D"],
  ["→", "\x1b[C"],
  ["Home", "\x1b[H"],
  ["End", "\x1b[F"],
  ["PgUp", "\x1b[5~"],
  ["PgDn", "\x1b[6~"],
  ["Ctrl+C", "\x03"],
  ["Ctrl+D", "\x04"],
  ["Ctrl+Z", "\x1a"],
  ["Ctrl+L", "\x0c"],
]) {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.disabled = true;
  b.addEventListener("click", () => sendRaw(seq));
  keysBar.appendChild(b);
}

async function connect(ticket, code) {
  try {
    setStatus("loading iroh (wasm)…");
    const mod = await import("./wasm/termrs_share_web.js");
    await mod.default();

    setStatus("connecting over iroh…");
    client = await mod.ShareClient.connect(
      ticket,
      code,
      term.cols,
      term.rows,
      (data) => term.write(data),
      (cols, rows) => term.resize(cols, rows),
      () => {
        setStatus("disconnected");
        setKeysEnabled(false);
      },
    );

    term.focus();
    setKeysEnabled(true);
    setStatus("connected — this view is live (type to control)");
  } catch (err) {
    setStatus("failed: " + err);
    setKeysEnabled(false);
    showJoin();
  }
}

document.querySelector("#join form").addEventListener("submit", (event) => {
  event.preventDefault();
  const ticket = ticketInput.value.trim();
  const code = codeInput.value.trim();
  if (!ticket) {
    setStatus("a ticket is required");
    return;
  }
  dialog.close();
  // Remember the ticket in the URL so a refresh/revisit resumes the
  // session (and the link stays shareable). No reload: hash-only change.
  const link = new URLSearchParams({ ticket, code });
  history.replaceState(null, "", "#" + link.toString());
  connect(ticket, code);
});

// Prefill from a share link and connect immediately (no click needed);
// with no ticket in the URL, open the join form instead of idling.
ticketInput.value = params.get("ticket") || "";
codeInput.value = params.get("code") || "";
if (ticketInput.value) {
  dialog.close();
  connect(ticketInput.value.trim(), codeInput.value.trim());
} else {
  showJoin();
}

window.addEventListener("beforeunload", () => {
  if (client) client.close();
});
