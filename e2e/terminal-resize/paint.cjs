// Both observers measure the same terminal pixels and two compositor frames.
// Avoid encoding unrelated page space, especially under the fixed CPU quota.
function terminalPainter(page) {
  const screen = page.locator('.xterm-screen');
  return async () => {
    await page.evaluate(() => window.fixture.frame());
    // Recompute the clip after resize; locator.screenshot adds an unrelated
    // actionability/stability wait to the measured rendering boundary.
    const clip = await screen.boundingBox();
    if (!clip) throw new Error('Terminal screen is not visible for pixel measurement');
    return page.screenshot({ clip, animations: 'disabled' });
  };
}

module.exports = { terminalPainter };
