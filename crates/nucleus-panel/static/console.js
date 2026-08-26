(function () {
  const el = document.getElementById("console-log");
  const input = document.getElementById("cmd");
  if (!el) return;
  const proto = location.protocol === "https:" ? "wss://" : "ws://";
  let ws = null;
  let reconnectTimer = null;

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
    const atBottom =
      el.scrollTop + el.clientHeight >= el.scrollHeight - 30;
    el.textContent += t + "\n";
    if (atBottom) el.scrollTop = el.scrollHeight;
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
