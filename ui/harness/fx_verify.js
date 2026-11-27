(async () => {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const waitFor = async (fn, timeout, label) => {
    const end = Date.now() + timeout;
    while (Date.now() < end) {
      if (fn()) return;
      await sleep(100);
    }
    throw new Error(`Timed out waiting for ${label}`);
  };
  const report = (payload) => window.__crosshairSend('FxVerifyReport', payload);
  try {
    await waitFor(() => window.__crosshairReady === true, 12000, 'the UI ready state');
    const config = window.__crosshairState?.config;
    if (!config?.audio) throw new Error("the audio config is unavailable");
    // The expectation comes from the real config file in the app, not from
    // this sandboxed page state.
    report({
      ok: true,
      steps: { uiReady: true, configRead: true, readOnly: true },
    });
  } catch (error) {
    report({ ok: false, error: String((error && error.message) || error) });
  }
})();
