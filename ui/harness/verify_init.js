
(function () {
  const errors = [];
  const consoleMessages = [];
  window.__crosshairVerifyErrors = errors;
  window.__crosshairVerifyConsole = consoleMessages;
  const text = (value) => (value && value.message) ? String(value.message) : String(value);
  window.addEventListener("error", (event) => errors.push("error: " + text(event.message || event.error || "unknown")));
  window.addEventListener("unhandledrejection", (event) => errors.push("unhandledrejection: " + text(event.reason)));
  ["log", "warn", "error"].forEach((level) => {
    const original = console[level];
    console[level] = function (...args) {
      consoleMessages.push(level + ": " + args.map(text).join(" "));
      return original.apply(this, args);
    };
  });
})();
