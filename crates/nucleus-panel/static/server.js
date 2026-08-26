(function () {
  "use strict";

  var SID = null;
  var dotEl = document.querySelector(".ssb-head .dot");
  var chipEl = document.querySelector(".srv-hero .chip");
  var powerGroup = document.getElementById("power-group");

  function getSid() {
    if (SID) return SID;
    if (powerGroup) SID = powerGroup.dataset.server;
    else {
      var m = location.pathname.match(/\/servers\/([^/]+)/);
      if (m) SID = m[1];
    }
    return SID;
  }

  var toastEl = document.getElementById("toast");
  var toastTimer = null;
  function toast(msg, kind) {
    if (!toastEl) return;
    toastEl.textContent = msg;
    toastEl.className = "toast show" + (kind === "error" ? " toast-error" : "");
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(function () {
      toastEl.className = "toast";
    }, 3000);
  }

  function syncPowerButtons(running) {
    if (!powerGroup) return;
    powerGroup.dataset.running = running ? "true" : "false";
    var btns = powerGroup.querySelectorAll("button[data-action]");
    btns.forEach(function (b) {
      var act = b.dataset.action;
      var enable =
        (act === "start" && !running) ||
        (act === "stop" && running) ||
        (act === "restart" && running) ||
        (act === "kill" && running);
      b.disabled = !enable;
    });
    if (dotEl) {
      dotEl.className = "dot " + (running ? "green" : "grey");
    }
    if (chipEl) {
      chipEl.className = "chip " + (running ? "green" : "");
      chipEl.textContent = running ? "Running" : "Stopped";
    }
  }

  var busy = false;
  function powerAction(action) {
    if (busy) return;
    var sid = getSid();
    if (!sid) return;
    busy = true;
    powerGroup.querySelectorAll("button").forEach(function (b) { b.disabled = true; });

    fetch("/servers/" + sid + "/power", {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: "action=" + encodeURIComponent(action),
    }).then(function (r) {
      if (r.ok) {
        toast(action.charAt(0).toUpperCase() + action.slice(1) + " sent");
      } else if (r.status === 403) {
        toast("Permission denied", "error");
      } else {
        toast("Error: " + r.status, "error");
      }
    }).catch(function () {
      toast("Network error", "error");
    }).finally(function () {
      busy = false;
    });
  }

  if (powerGroup) {
    powerGroup.addEventListener("click", function (e) {
      var btn = e.target.closest("button[data-action]");
      if (!btn || btn.disabled) return;
      e.preventDefault();
      powerAction(btn.dataset.action);
    });
  }

  function pollStatus() {
    var sid = getSid();
    if (!sid) return;
    fetch("/servers/" + sid + "/stats")
      .then(function (r) { return r.json(); })
      .then(function (s) {
        if (s && typeof s.running !== "undefined") {
          syncPowerButtons(s.running);
        }
      })
      .catch(function () {});
  }

  var pollTimer = setInterval(pollStatus, 3000);

  document.addEventListener("DOMContentLoaded", function () { pollStatus(); });

  var ajaxForms = document.querySelectorAll("form[data-ajax]");
  ajaxForms.forEach(function (form) {
    form.addEventListener("submit", function (e) {
      e.preventDefault();
      e.stopPropagation();
      var btn = form.querySelector("[type=submit]");
      if (btn) btn.disabled = true;
      var formData = new FormData(form);
      fetch(form.action, {
        method: form.method || "POST",
        body: formData,
      }).then(function (r) {
        if (r.ok) {
          var msg = form.dataset.ajaxOk || "Done";
          toast(msg);
          if (form.dataset.ajaxRefresh === "true") {
            setTimeout(function () { location.reload(); }, 800);
          }
        } else if (r.status === 403) {
          toast("Permission denied", "error");
        } else {
          toast("Error: " + r.status, "error");
        }
      }).catch(function () {
        toast("Network error", "error");
      }).finally(function () {
        if (btn) btn.disabled = false;
      });
    });
  });
})();
