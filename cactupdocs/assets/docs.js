/* CactupDocs client-side behavior: theme toggle, mobile nav, search,
   code-copy. Vanilla JS, dependency-free, guards for missing elements. */
(function () {
  "use strict";

  var THEME_KEY = "cactupdocs-theme";
  var root = document.documentElement;

  /* ---------- Theme ---------- */
  function systemTheme() {
    return window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  function currentTheme() {
    return root.getAttribute("data-theme") ||
      localStorage.getItem(THEME_KEY) || systemTheme();
  }
  function applyTheme(theme) {
    root.setAttribute("data-theme", theme);
    var btn = document.getElementById("theme-toggle");
    if (btn) {
      var icon = btn.querySelector(".theme-icon") || btn;
      icon.textContent = theme === "dark" ? "☀" : "🌙";
      btn.setAttribute("aria-label",
        theme === "dark" ? "Switch to light theme" : "Switch to dark theme");
    }
  }
  applyTheme(localStorage.getItem(THEME_KEY) || systemTheme());

  var themeBtn = document.getElementById("theme-toggle");
  if (themeBtn) {
    themeBtn.addEventListener("click", function () {
      var next = currentTheme() === "dark" ? "light" : "dark";
      localStorage.setItem(THEME_KEY, next);
      applyTheme(next);
    });
  }

  /* ---------- Mobile nav ---------- */
  var sidebar = document.querySelector(".sidebar");
  var navToggle = document.getElementById("nav-toggle");
  var scrim = null;
  if (sidebar) {
    scrim = document.createElement("div");
    scrim.className = "nav-scrim";
    document.body.appendChild(scrim);
  }
  function closeNav() {
    if (sidebar) sidebar.classList.remove("open");
    if (scrim) scrim.classList.remove("open");
  }
  if (navToggle && sidebar) {
    navToggle.addEventListener("click", function () {
      var open = sidebar.classList.toggle("open");
      if (scrim) scrim.classList.toggle("open", open);
    });
    if (scrim) scrim.addEventListener("click", closeNav);
    sidebar.addEventListener("click", function (e) {
      if (e.target.tagName === "A") closeNav();
    });
  }

  /* ---------- Search ---------- */
  var input = document.getElementById("search-input");
  var results = document.getElementById("search-results");
  var indexUrl = document.body ? document.body.getAttribute("data-search-index") : null;
  var index = null;
  var loading = false;

  function loadIndex() {
    if (index || loading || !indexUrl) return;
    loading = true;
    fetch(indexUrl)
      .then(function (r) { return r.ok ? r.json() : []; })
      .then(function (data) { index = Array.isArray(data) ? data : []; })
      .catch(function () { index = []; });
  }

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }

  function snippet(text, q) {
    if (!text) return "";
    var i = text.toLowerCase().indexOf(q);
    if (i < 0) return text.slice(0, 100);
    var start = Math.max(0, i - 30);
    return (start > 0 ? "…" : "") + text.slice(start, start + 120) + "…";
  }

  function render(query) {
    if (!results) return;
    var q = query.trim().toLowerCase();
    if (!q || !index) { results.classList.remove("open"); results.innerHTML = ""; return; }
    var hits = [];
    for (var i = 0; i < index.length && hits.length < 10; i++) {
      var e = index[i];
      var hay = ((e.title || "") + " " + (e.text || "")).toLowerCase();
      if (hay.indexOf(q) >= 0) hits.push(e);
    }
    if (!hits.length) {
      results.innerHTML = '<div class="r-empty">No matches</div>';
      results.classList.add("open");
      return;
    }
    results.innerHTML = hits.map(function (e) {
      return '<a href="' + escapeHtml(e.url) + '">' +
        '<span class="r-title">' + escapeHtml(e.title || e.url) + '</span>' +
        '<span class="r-snippet">' + escapeHtml(snippet(e.text || "", q)) + '</span></a>';
    }).join("");
    results.classList.add("open");
  }

  var debounce;
  if (input) {
    input.addEventListener("focus", loadIndex);
    input.addEventListener("input", function () {
      clearTimeout(debounce);
      var v = input.value;
      debounce = setTimeout(function () { render(v); }, 120);
    });
    input.addEventListener("keydown", function (e) {
      if (e.key === "Escape") { input.value = ""; render(""); input.blur(); }
    });
    document.addEventListener("click", function (e) {
      if (results && !results.contains(e.target) && e.target !== input) {
        results.classList.remove("open");
      }
    });
  }

  /* ---------- Copy buttons ---------- */
  document.querySelectorAll("pre").forEach(function (pre) {
    var code = pre.querySelector("code");
    if (!code) return;
    var btn = document.createElement("button");
    btn.className = "copy-btn";
    btn.type = "button";
    btn.textContent = "Copy";
    btn.addEventListener("click", function () {
      var text = code.innerText;
      var done = function () { btn.textContent = "Copied!"; setTimeout(function () { btn.textContent = "Copy"; }, 1500); };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done).catch(function () {});
      } else {
        var ta = document.createElement("textarea");
        ta.value = text; document.body.appendChild(ta); ta.select();
        try { document.execCommand("copy"); done(); } catch (e) {}
        document.body.removeChild(ta);
      }
    });
    pre.appendChild(btn);
  });
})();
