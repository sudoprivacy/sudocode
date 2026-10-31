// Both observers share completed terminal PNG readback and two compositor frames.
const assert = require('node:assert/strict');
const { performance } = require('node:perf_hooks');

async function terminalPainter(page) {
  const readback = await page.context().newCDPSession(page);
  const environment = { method: 'Page.captureScreenshot', optimizeForSpeed: true };
  const observations = [];
  const capture = async () => {
    const started = performance.now();
    const { clip, frameMs, boundsMs } = await page.evaluate(async () => {
      const began = performance.now();
      await document.fonts.ready;
      await window.fixture.frame();
      const framed = performance.now();
      if (document.getAnimations().some(animation => animation.playState === 'running')) {
        throw new Error('Terminal painting measurements require non-animated CSS');
      }
      const clip = window.fixture.pixelBounds();
      return { clip, frameMs: framed - began, boundsMs: performance.now() - framed };
    });
    assert(clip.width > 0 && clip.height > 0, 'missing terminal pixel bounds');
    const ready = performance.now();
    // Read the completed PNG directly. Generic page screenshots prepare
    // animation/caret state and repeat layout work before capturing pixels.
    const { data } = await readback.send('Page.captureScreenshot', {
      format: 'png', clip: { ...clip, scale: 1 },
      captureBeyondViewport: false, optimizeForSpeed: true,
    });
    observations.push({ frame_ms: frameMs, bounds_ms: boundsMs,
      ready_rpc_ms: ready - started, readback_ms: performance.now() - ready,
      total_ms: performance.now() - started });
    return Buffer.from(data, 'base64');
  };
  return { capture, observations, environment };
}

module.exports = { terminalPainter };
