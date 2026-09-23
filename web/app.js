// lrmux web client — binary protocol over WebSocket.
// Same framing as Unix/TCP: [u32 LE length][u8 type][payload].

const C_IDENTIFY = 0x01;
const C_PANE_INPUT = 0x02;
const C_RESIZE = 0x03;
const C_DETACH = 0x04;

const S_IDENTIFY_ACK = 0x10;
const S_GRID_SNAPSHOT = 0x11;
const S_GRID_UPDATE = 0x12;
const S_SCROLLBACK_UPDATE = 0x17;
const S_PANE_EXIT = 0x13;
const S_ERROR = 0x14;
const S_STATUS_BAR = 0x15;

const $ = (id) => document.getElementById(id);
const termEl = $("term");
const statusBarEl = $("statusbar");
const statusEl = $("status");

let ws = null;
let recvBuf = new Uint8Array(0);
let grid = { rows: 24, cols: 80, cells: [], cursor: { r: 0, c: 0, vis: true } };
let attached = false;

function setStatus(msg, err = false) {
  statusEl.textContent = msg;
  statusEl.classList.toggle("err", err);
}

function concat(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

function u16(n) {
  const b = new Uint8Array(2);
  new DataView(b.buffer).setUint16(0, n, true);
  return b;
}

function u32(n) {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, n, true);
  return b;
}

function encodeFrame(type, payload) {
  const body = new Uint8Array(1 + payload.length);
  body[0] = type;
  body.set(payload, 1);
  const frame = new Uint8Array(4 + body.length);
  frame.set(u32(body.length), 0);
  frame.set(body, 4);
  return frame;
}

function encodeIdentify(rows, cols, psk) {
  const enc = new TextEncoder();
  const token = enc.encode(psk || "");
  const payload = new Uint8Array(2 + 2 + 1 + 4 + token.length);
  let o = 0;
  payload.set(u16(rows), o); o += 2;
  payload.set(u16(cols), o); o += 2;
  payload[o++] = 1; // attach
  payload.set(u32(token.length), o); o += 4;
  payload.set(token, o);
  return encodeFrame(C_IDENTIFY, payload);
}

function encodePaneInput(bytes) {
  return encodeFrame(C_PANE_INPUT, bytes);
}

function encodeResize(rows, cols) {
  const payload = new Uint8Array(4);
  payload.set(u16(rows), 0);
  payload.set(u16(cols), 2);
  return encodeFrame(C_RESIZE, payload);
}

function readU16(view, o) {
  return { v: view.getUint16(o, true), o: o + 2 };
}
function readU32(view, o) {
  return { v: view.getUint32(o, true), o: o + 4 };
}
function readBytes(buf, o, n) {
  return { v: buf.subarray(o, o + n), o: o + n };
}
function readString(buf, view, o) {
  const len = readU32(view, o);
  o = len.o;
  const bytes = readBytes(buf, o, len.v);
  return { v: new TextDecoder().decode(bytes.v), o: bytes.o };
}

function decodeColor(buf, o) {
  const kind = buf[o++];
  if (kind === 0) return { color: null, o };
  if (kind === 1) return { color: { indexed: buf[o] }, o: o + 1 };
  if (kind === 2) {
    return { color: { rgb: [buf[o], buf[o + 1], buf[o + 2]] }, o: o + 3 };
  }
  return { color: null, o };
}

function decodeCell(buf, o) {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const ch = readU32(view, o);
  o = ch.o;
  const fg = decodeColor(buf, o);
  o = fg.o;
  const bg = decodeColor(buf, o);
  o = bg.o;
  const attrs = buf[o++];
  return {
    cell: {
      ch: ch.v ? String.fromCodePoint(ch.v) : " ",
      fg: fg.color,
      bg: bg.color,
      bold: !!(attrs & 1),
      italic: !!(attrs & 2),
      underline: !!(attrs & 4),
      reverse: !!(attrs & 8),
    },
    o,
  };
}

function emptyCell() {
  return { ch: " ", fg: null, bg: null, bold: false, italic: false, underline: false, reverse: false };
}

function allocGrid(rows, cols) {
  const cells = new Array(rows * cols);
  for (let i = 0; i < cells.length; i++) cells[i] = emptyCell();
  grid = { rows, cols, cells, cursor: { r: 0, c: 0, vis: true } };
}

function cssColor(c) {
  if (!c) return null;
  if (c.rgb) return `rgb(${c.rgb[0]},${c.rgb[1]},${c.rgb[2]})`;
  if (c.indexed != null) {
    // rough 256-color approximation for the common 16
    const palette = [
      "#000","#a00","#0a0","#a60","#00a","#a0a","#0aa","#aaa",
      "#555","#f55","#5f5","#ff5","#55f","#f5f","#5ff","#fff",
    ];
    return palette[c.indexed] || "#ccc";
  }
  return null;
}

function render() {
  const { rows, cols, cells, cursor } = grid;
  let html = "";
  for (let r = 0; r < rows; r++) {
    for (let c = 0; c < cols; c++) {
      const cell = cells[r * cols + c] || emptyCell();
      let fg = cssColor(cell.fg) || "#e6edf3";
      let bg = cssColor(cell.bg);
      if (cell.reverse) {
        const t = fg;
        fg = bg || "#e6edf3";
        bg = t;
      }
      const isCursor = cursor.vis && cursor.r === r && cursor.c === c;
      if (isCursor) {
        bg = bg || "#58a6ff";
        fg = "#0d1117";
      }
      let style = `color:${fg}`;
      if (bg) style += `;background:${bg}`;
      if (cell.bold) style += ";font-weight:700";
      if (cell.italic) style += ";font-style:italic";
      if (cell.underline) style += ";text-decoration:underline";
      const ch = cell.ch === " " ? "\u00a0" : cell.ch;
      html += `<span style="${style}">${escapeHtml(ch)}</span>`;
    }
    html += "\n";
  }
  termEl.innerHTML = html;
}

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function handleServerPayload(tag, data) {
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  let o = 0;
  if (tag === S_IDENTIFY_ACK) {
    const rows = readU16(view, o); o = rows.o;
    const cols = readU16(view, o); o = cols.o;
    const ver = readString(data, view, o); o = ver.o;
    const addr = o < data.length ? readString(data, view, o).v : "";
    allocGrid(rows.v, cols.v);
    attached = true;
    setStatus(`connected — ${ver.v} @ ${addr} (${rows.v}x${cols.v})`);
    render();
    return;
  }
  if (tag === S_GRID_SNAPSHOT) {
    const rows = readU16(view, o); o = rows.o;
    const cols = readU16(view, o); o = cols.o;
    const n = readU32(view, o); o = n.o;
    allocGrid(rows.v, cols.v);
    for (let i = 0; i < n.v && i < grid.cells.length; i++) {
      const cell = decodeCell(data, o);
      o = cell.o;
      grid.cells[i] = cell.cell;
    }
    const cr = readU16(view, o); o = cr.o;
    const cc = readU16(view, o); o = cc.o;
    grid.cursor = { r: cr.v, c: cc.v, vis: data[o] !== 0 };
    render();
    return;
  }
  if (tag === S_GRID_UPDATE) {
    const n = readU32(view, o); o = n.o;
    for (let i = 0; i < n.v; i++) {
      const row = readU16(view, o); o = row.o;
      const count = readU32(view, o); o = count.o;
      for (let c = 0; c < count.v; c++) {
        const cell = decodeCell(data, o);
        o = cell.o;
        const idx = row.v * grid.cols + c;
        if (idx < grid.cells.length) grid.cells[idx] = cell.cell;
      }
    }
    const cr = readU16(view, o); o = cr.o;
    const cc = readU16(view, o); o = cc.o;
    grid.cursor = { r: cr.v, c: cc.v, vis: data[o] !== 0 };
    render();
    return;
  }
  if (tag === S_SCROLLBACK_UPDATE) {
    // Live scroll lines are for native scrollback; ignore in web MVP.
    return;
  }
  if (tag === S_STATUS_BAR) {
    const session = readString(data, view, o); o = session.o;
    const wcount = readU32(view, o); o = wcount.o;
    const windows = [];
    for (let i = 0; i < wcount.v; i++) {
      const w = readString(data, view, o);
      o = w.o;
      windows.push(w.v);
    }
    const active = readU16(view, o); o = active.o;
    const scount = readU16(view, o); o = scount.o;
    if (o < data.length) o += 1; // high_output
    let server = "";
    if (o + 4 <= data.length) server = readString(data, view, o).v;
    const parts = windows.map((w, i) => (i === active.v ? `*${w}` : w));
    const label = server ? `[${session.v}]@${server}` : session.v;
    statusBarEl.textContent = `${label} [${scount.v}] ${parts.join(" ")}`;
    return;
  }
  if (tag === S_ERROR) {
    const msg = readString(data, view, 0).v;
    setStatus(`error: ${msg}`, true);
    return;
  }
  if (tag === S_PANE_EXIT) {
    setStatus(`pane exited (code ${data[0]})`, true);
  }
}

function ingest(bytes) {
  recvBuf = concat(recvBuf, bytes);
  while (recvBuf.length >= 4) {
    const view = new DataView(recvBuf.buffer, recvBuf.byteOffset, recvBuf.byteLength);
    const len = view.getUint32(0, true);
    if (len === 0 || recvBuf.length < 4 + len) break;
    const frame = recvBuf.subarray(4, 4 + len);
    recvBuf = recvBuf.subarray(4 + len);
    const tag = frame[0];
    const payload = frame.subarray(1);
    handleServerPayload(tag, payload);
  }
}

function termSize() {
  // Approximate size from CSS metrics.
  const cs = getComputedStyle(termEl);
  const fontSize = parseFloat(cs.fontSize) || 13;
  const lineHeight = fontSize * 1.2;
  const charWidth = fontSize * 0.6;
  const rows = Math.max(8, Math.floor(termEl.clientHeight / lineHeight));
  const cols = Math.max(20, Math.floor(termEl.clientWidth / charWidth));
  return { rows, cols };
}

function connect() {
  if (ws) return;
  const url = $("url").value.trim();
  const psk = $("psk").value;
  setStatus("connecting…");
  ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  $("connect").disabled = true;
  $("disconnect").disabled = false;

  ws.onopen = () => {
    const { rows, cols } = termSize();
    // Include status bar row like the native client.
    ws.send(encodeIdentify(rows + 1, cols, psk));
    setStatus("handshaking…");
    termEl.focus();
  };
  ws.onmessage = (ev) => {
    ingest(new Uint8Array(ev.data));
  };
  ws.onerror = () => setStatus("WebSocket error", true);
  ws.onclose = () => {
    ws = null;
    attached = false;
    $("connect").disabled = false;
    $("disconnect").disabled = true;
    setStatus("disconnected");
  };
}

function disconnect() {
  if (!ws) return;
  try {
    ws.send(encodeFrame(C_DETACH, new Uint8Array(0)));
  } catch (_) {}
  ws.close();
}

$("connect").onclick = connect;
$("disconnect").onclick = disconnect;

termEl.addEventListener("keydown", (e) => {
  if (!ws || ws.readyState !== WebSocket.OPEN || !attached) return;
  // Let the browser handle refresh etc. with modifiers alone.
  if (e.metaKey || e.altKey) return;
  e.preventDefault();
  let bytes;
  if (e.key === "Enter") bytes = new Uint8Array([0x0d]);
  else if (e.key === "Backspace") bytes = new Uint8Array([0x7f]);
  else if (e.key === "Tab") bytes = new Uint8Array([0x09]);
  else if (e.key === "Escape") bytes = new Uint8Array([0x1b]);
  else if (e.key === "ArrowUp") bytes = new Uint8Array([0x1b, 0x5b, 0x41]);
  else if (e.key === "ArrowDown") bytes = new Uint8Array([0x1b, 0x5b, 0x42]);
  else if (e.key === "ArrowRight") bytes = new Uint8Array([0x1b, 0x5b, 0x43]);
  else if (e.key === "ArrowLeft") bytes = new Uint8Array([0x1b, 0x5b, 0x44]);
  else if (e.key.length === 1) {
    if (e.ctrlKey) {
      const code = e.key.toUpperCase().charCodeAt(0) - 64;
      if (code >= 0 && code < 32) bytes = new Uint8Array([code]);
    } else {
      bytes = new TextEncoder().encode(e.key);
    }
  }
  if (bytes) ws.send(encodePaneInput(bytes));
});

window.addEventListener("resize", () => {
  if (!ws || ws.readyState !== WebSocket.OPEN || !attached) return;
  const { rows, cols } = termSize();
  ws.send(encodeResize(rows + 1, cols));
});
