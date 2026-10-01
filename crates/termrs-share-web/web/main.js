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
      () => setStatus("disconnected"),
    );

    term.focus();
    setStatus("connected — this view is live (type to control)");
  } catch (err) {
    setStatus("failed: " + err);
    dialog.showModal();
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
  connect(ticket, code);
});

// Prefill from a share link and connect immediately (no click needed).
ticketInput.value = params.get("ticket") || "";
codeInput.value = params.get("code") || "";
if (ticketInput.value) {
  dialog.close();
  connect(ticketInput.value.trim(), codeInput.value.trim());
}

window.addEventListener("beforeunload", () => {
  if (client) client.close();
});
