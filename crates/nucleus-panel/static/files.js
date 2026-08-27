(function () {
  "use strict";
  var SID = (location.pathname.match(/\/servers\/([^/]+)/) || [])[1] || "";
  var cwd = new URLSearchParams(location.search).get("path") || "/";
  var toastEl = document.getElementById("toast");
  function toast(msg, kind) {
    if (!toastEl) return;
    toastEl.textContent = msg;
    toastEl.className = "toast show" + (kind === "error" ? " toast-error" : "");
    setTimeout(function () { toastEl.className = "toast"; }, 2500);
  }
  function getChecked() {
    return Array.from(document.querySelectorAll(".row-check:checked")).map(function (c) { return c.dataset.path; });
  }
  function singleSelected() {
    var c = getChecked();
    return c.length === 1 ? c[0] : null;
  }
  function refresh() { location.reload(); }
  function api(path, body) {
    return fetch("/servers/" + SID + path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }).then(function (r) {
      if (r.ok) return r.json().catch(function () { return { ok: true }; });
      return r.text().then(function (t) { throw new Error(t); });
    });
  }

  var selectAll = document.getElementById("select-all");
  var checks = document.querySelectorAll(".row-check");
  selectAll.addEventListener("change", function () {
    checks.forEach(function (c) { c.checked = selectAll.checked; });
    updateButtonStates();
  });
  checks.forEach(function (c) { c.addEventListener("change", updateButtonStates); });

  function updateButtonStates() {
    var n = getChecked().length;
    ["btn-rename", "btn-move", "btn-archive"].forEach(function (id) {
      document.getElementById(id).disabled = n !== 1;
    });
    document.getElementById("btn-extract").disabled = n !== 1;
    document.getElementById("btn-delete-multi").disabled = n === 0;
  }

  function show(id) { document.getElementById(id).hidden = false; }
  function hide(id) { document.getElementById(id).hidden = true; }

  document.getElementById("btn-newfolder").addEventListener("click", function () { show("newfolder-form"); document.getElementById("newfolder-name").focus(); });
  document.getElementById("newfolder-cancel").addEventListener("click", function () { hide("newfolder-form"); });
  document.getElementById("newfolder-go").addEventListener("click", function () {
    var name = document.getElementById("newfolder-name").value.trim();
    if (!name) return;
    api("/files/mkdir", { path: cwd === "/" ? "/" + name : cwd + "/" + name }).then(function () { toast("Folder created"); setTimeout(refresh, 600); }).catch(function (e) { toast("Error: " + e.message, "error"); });
  });

  document.getElementById("btn-fetch").addEventListener("click", function () { show("fetch-form"); document.getElementById("fetch-url").focus(); });
  document.getElementById("fetch-cancel").addEventListener("click", function () { hide("fetch-form"); });
  document.getElementById("fetch-go").addEventListener("click", function () {
    var url = document.getElementById("fetch-url").value.trim();
    if (!url) return;
    api("/files/fetch", { url: url, path: cwd }).then(function () { toast("Downloaded"); setTimeout(refresh, 800); }).catch(function (e) { toast("Error: " + e.message, "error"); });
  });

  var fileInput = document.getElementById("file-input");
  document.getElementById("btn-upload").addEventListener("click", function () { fileInput.click(); });
  fileInput.addEventListener("change", function () {
    var files = fileInput.files;
    if (!files.length) return;
    var done = 0, fail = 0;
    Array.from(files).forEach(function (f) {
      var fd = new FormData();
      fd.append("file", f);
      fd.append("cwd", cwd);
      fetch("/servers/" + SID + "/files/upload", { method: "POST", body: fd })
        .then(function (r) { if (r.ok) done++; else fail++; })
        .catch(function () { fail++; })
        .finally(function () {
          if (done + fail === files.length) {
            if (fail === 0) toast(done + " file(s) uploaded");
            else toast(done + " ok, " + fail + " failed", "error");
            setTimeout(refresh, 800);
          }
        });
    });
  });

  // drag-drop
  var dropZone = document.getElementById("drop-zone");
  var dropPath = document.getElementById("drop-path");
  document.addEventListener("dragover", function (e) { e.preventDefault(); dropZone.hidden = false; dropPath.textContent = cwd; });
  document.addEventListener("dragleave", function (e) { if (e.target === document.documentElement) dropZone.hidden = true; });
  document.addEventListener("drop", function (e) {
    e.preventDefault();
    dropZone.hidden = true;
    var files = e.dataTransfer.files;
    if (!files.length) return;
    Array.from(files).forEach(function (f) {
      var fd = new FormData();
      fd.append("file", f);
      fd.append("cwd", cwd);
      fetch("/servers/" + SID + "/files/upload", { method: "POST", body: fd });
    });
    toast("Uploading " + files.length + " file(s)…");
    setTimeout(refresh, 1500);
  });

  // rename
  document.getElementById("btn-rename").addEventListener("click", function () {
    var p = singleSelected();
    if (!p) return;
    document.getElementById("rename-to").value = p.split("/").pop();
    show("rename-form");
    document.getElementById("rename-to").focus();
  });
  document.getElementById("rename-cancel").addEventListener("click", function () { hide("rename-form"); });
  document.getElementById("rename-go").addEventListener("click", function () {
    var from = singleSelected();
    var name = document.getElementById("rename-to").value.trim();
    if (!from || !name) return;
    var dir = from.substring(0, from.lastIndexOf("/"));
    var to = dir + "/" + name;
    api("/files/rename", { from: from, to: to }).then(function () { toast("Renamed"); setTimeout(refresh, 600); }).catch(function (e) { toast("Error: " + e.message, "error"); });
  });

  // move
  document.getElementById("btn-move").addEventListener("click", function () {
    var p = singleSelected();
    if (!p) return;
    document.getElementById("move-to").value = cwd;
    show("move-form");
    document.getElementById("move-to").focus();
  });
  document.getElementById("move-cancel").addEventListener("click", function () { hide("move-form"); });
  document.getElementById("move-go").addEventListener("click", function () {
    var from = singleSelected();
    var dir = document.getElementById("move-to").value.trim();
    if (!from || !dir) return;
    var name = from.split("/").pop();
    var to = dir.endsWith("/") ? dir + name : dir + "/" + name;
    api("/files/move", { from: from, to: to }).then(function () { toast("Moved"); setTimeout(refresh, 600); }).catch(function (e) { toast("Error: " + e.message, "error"); });
  });

  // archive
  document.getElementById("btn-archive").addEventListener("click", function () {
    var p = singleSelected();
    if (!p) return;
    api("/files/archive", { path: p, action: "tar.gz" }).then(function () { toast("Archive created"); setTimeout(refresh, 600); }).catch(function (e) { toast("Error: " + e.message, "error"); });
  });

  // extract
  document.getElementById("btn-extract").addEventListener("click", function () {
    var p = singleSelected();
    if (!p) return;
    api("/files/extract", { path: p, action: "extract" }).then(function () { toast("Extracted"); setTimeout(refresh, 600); }).catch(function (e) { toast("Error: " + e.message, "error"); });
  });

  // multi-delete
  document.getElementById("btn-delete-multi").addEventListener("click", function () {
    var paths = getChecked();
    if (!paths.length) return;
    if (!confirm("Delete " + paths.length + " item(s)?")) return;
    var done = 0;
    paths.forEach(function (p) {
      var fd = new FormData();
      fd.append("path", p);
      fetch("/servers/" + SID + "/files/delete", { method: "POST", body: fd })
        .finally(function () { done++; if (done === paths.length) { toast("Deleted"); setTimeout(refresh, 600); } });
    });
  });

  // mod browser
  var modBrowser = document.getElementById("mod-browser");
  document.getElementById("btn-mods").addEventListener("click", function () { modBrowser.hidden = !modBrowser.hidden; if (!modBrowser.hidden) document.getElementById("mod-search").focus(); });
  document.getElementById("mod-close").addEventListener("click", function () { modBrowser.hidden = true; });
  document.getElementById("mod-search-btn").addEventListener("click", searchMods);
  document.getElementById("mod-search").addEventListener("keydown", function (e) { if (e.key === "Enter") searchMods(); });

  function searchMods() {
    var q = document.getElementById("mod-search").value.trim();
    var loader = document.getElementById("mod-loader").value;
    if (!q) return;
    var results = document.getElementById("mod-results");
    results.innerHTML = '<p class="muted">Searching…</p>';
    fetch("/servers/" + SID + "/mods/search?q=" + encodeURIComponent(q) + "&loader=" + loader)
      .then(function (r) { return r.json(); })
      .then(function (data) {
        var hits = data.hits || [];
        if (!hits.length) { results.innerHTML = '<p class="muted">No mods found.</p>'; return; }
        results.innerHTML = hits.map(function (h) {
          return '<div class="mod-item">' +
            '<div class="mod-info"><strong>' + (h.title || h.slug) + '</strong>' +
            '<p class="muted small">' + (h.description || "").substring(0, 120) + '</p>' +
            '<p class="muted small">⬇ ' + (h.downloads || 0) + ' · ' + (h.project_type || "mod") + '</p></div>' +
            '<button class="btn btn-sm btn-primary mod-install" data-pid="' + h.project_id + '">Install</button>' +
            '</div>';
        }).join("");
        document.querySelectorAll(".mod-install").forEach(function (btn) {
          btn.addEventListener("click", function () {
            btn.disabled = true;
            btn.textContent = "Installing…";
            fetch("/servers/" + SID + "/mods/install", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({ project_id: btn.dataset.pid, game_version: "*", loader: document.getElementById("mod-loader").value }),
            }).then(function (r) { return r.json(); }).then(function (r) {
              if (r.ok) { btn.textContent = "✓ Installed"; toast("Mod installed to /mods/"); }
              else { btn.textContent = "Install"; toast("Install failed", "error"); btn.disabled = false; }
            }).catch(function () { btn.textContent = "Install"; toast("Network error", "error"); btn.disabled = false; });
          });
        });
      })
      .catch(function () { results.innerHTML = '<p class="error">Search failed.</p>'; });
  }
})();
