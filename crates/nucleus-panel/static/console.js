(function () {
  const el = document.getElementById("console-log");
  const input = document.getElementById("cmd");
  if (!el) return;
  const proto = location.protocol === "https:" ? "wss://" : "ws://";
  let ws = null;
  let reconnectTimer = null;

  // Buffering keeps us off the per-message layout path: a busy server can
  // emit hundreds of lines a second, and each full re-wrap of the log inside
  // a backdrop-blurred panel is expensive. Flush at most ~10x/sec and append
  // text at the end only, so the browser never re-copies the whole node.
  const MAX_LOG_CHARS = 512 * 1024;
  const TRIM_TO_CHARS = 256 * 1024;
  const FLUSH_MS = 100;
  let pending = [];
  let flushTimer = null;

  function flush() {
    flushTimer = null;
    if (!pending.length) return;
    const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 30;
    el.insertAdjacentText("beforeend", pending.join("\n") + "\n");
    pending.length = 0;
    if (el.textContent.length > MAX_LOG_CHARS) {
      el.textContent = el.textContent.slice(-TRIM_TO_CHARS);
    }
    if (atBottom) el.scrollTop = el.scrollHeight;
  }

  function scheduleFlush() {
    if (flushTimer) return;
    flushTimer = setTimeout(flush, FLUSH_MS);
  }

  function connect() {
    ws = new WebSocket(proto + location.host + window.NUCLEUS_WS);
    ws.onopen = () => line("[connected]");
    ws.onmessage = (e) => line(e.data);
    ws.onclose = () => {
      line("[disconnected — retrying in 3s]");
      clearTimeout(reconnectTimer);
      reconnectTimer = setTimeout(connect, 3000);
    };
    ws.onerror = () => ws.close();
  }

  function clean(t) {
    return t
      .replace(/\u001b\[[0-9;?]*[@-~]/g, "")
      .replace(/\u001b\][^\u0007\u001b]*(\u0007|\u001b\\)/g, "")
      .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, "");
  }

  function line(raw) {
    const t = clean(raw);
    if (!t) return;
    pending.push(t);
    scheduleFlush();
  }

  function send() {
    const cmd = input.value.trim();
    if (!cmd || !ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(cmd);
    line("> " + cmd);
    input.value = "";
  }

  document.getElementById("send").addEventListener("click", send);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") send();
  });

  connect();
})();
