// Click-to-copy for the install command boxes. Falls back to a hidden
// textarea + execCommand on browsers without the async clipboard API
// (or when served over plain http, where navigator.clipboard is undefined).
(function () {
  "use strict";

  function flash(btn) {
    var original = btn.textContent;
    btn.textContent = "Copied!";
    btn.classList.add("copied");
    setTimeout(function () {
      btn.textContent = original;
      btn.classList.remove("copied");
    }, 1500);
  }

  function legacyCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "absolute";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    ta.select();
    var ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (e) {
      ok = false;
    }
    document.body.removeChild(ta);
    return ok;
  }

  function copy(text, btn) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(
        function () { flash(btn); },
        function () { if (legacyCopy(text)) flash(btn); }
      );
    } else if (legacyCopy(text)) {
      flash(btn);
    }
  }

  function wire(btnId, textId) {
    var btn = document.getElementById(btnId);
    var src = document.getElementById(textId);
    if (!btn || !src) return;
    btn.addEventListener("click", function () {
      copy(src.textContent.trim(), btn);
    });
  }

  wire("copy-btn", "cmd-text");
  wire("copy-wget-btn", "cmd-wget-text");
})();
