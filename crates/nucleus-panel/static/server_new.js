(function () {
  const select = document.getElementById("egg-select");
  const image = document.getElementById("image-input");
  const startup = document.getElementById("startup-input");
  const varsGrid = document.getElementById("egg-vars");

  // ---- mode switch (modpack vs custom/egg) ----
  const modeField = document.getElementById("mode-field");
  const sectionModpack = document.getElementById("section-modpack");
  const sectionCustom = document.getElementById("section-custom");
  document.querySelectorAll(".mode-btn").forEach((btn) => {
    btn.addEventListener("click", () => setMode(btn.dataset.mode));
  });
  function setMode(mode) {
    modeField.value = mode;
    document.querySelectorAll(".mode-btn").forEach((b) =>
      b.classList.toggle("active", b.dataset.mode === mode)
    );
    sectionModpack.style.display = mode === "modpack" ? "" : "none";
    sectionCustom.style.display = mode === "custom" ? "" : "none";
    // hidden sections must not submit values at all
    sectionModpack.querySelectorAll("input,select,textarea").forEach((el) => {
      el.disabled = mode !== "modpack";
    });
    sectionCustom.querySelectorAll("input,select,textarea").forEach((el) => {
      el.disabled = mode !== "custom";
    });
    const packInput = sectionModpack.querySelector("input[type=file]");
    if (packInput) packInput.required = mode === "modpack";
    image.required = mode === "custom";
  }
  setMode("modpack");

  if (!select) return;

  let eggs = [];
  try {
    eggs = JSON.parse(document.getElementById("eggs-data").textContent || "[]");
  } catch (e) {
    eggs = [];
  }

  function apply() {
    varsGrid.innerHTML = "";
    if (select.value === "custom") {
      image.readOnly = false;
      startup.readOnly = false;
      return;
    }
    const egg = eggs.find((e) => e.slug === select.value);
    if (!egg) return;
    if (egg.images && egg.images.length > 0) {
      image.value = egg.images[0];
      image.readOnly = false;
    }
    startup.value = egg.startup;
    startup.readOnly = true;
    for (const v of egg.vars || []) {
      if (v.env === "SERVER_MEMORY" || v.env === "SERVER_PORT") continue;
      const wrap = document.createElement("label");
      wrap.textContent = v.name + (v.required ? " *" : "");
      const input = document.createElement("input");
      input.name = "var_" + v.env;
      input.dataset.env = v.env;
      input.value = v.default || "";
      wrap.appendChild(input);
      varsGrid.appendChild(wrap);
    }
  }

  select.addEventListener("change", apply);
  apply();
})();
