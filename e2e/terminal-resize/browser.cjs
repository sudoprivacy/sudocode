// Dedicated browser observers use one resource envelope and attest their backend.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { createHash } = require('node:crypto');

const RASTER_THREADS = 1;
const RASTER_SWITCH = '--num-raster-threads';
const rasterArgument = `${RASTER_SWITCH}=${RASTER_THREADS}`;
const implementationSha256 = createHash('sha256').update(fs.readFileSync(__filename)).digest('hex');

/** Launch a dedicated observer and report its actual resource/backend identity. */
async function launchBrowser(chromium, options = {}) {
  assert(!(options.args || []).some(arg => arg.startsWith(RASTER_SWITCH)),
    'Raster worker ownership belongs to the shared browser observer');
  const browser = await chromium.launch({ ...options,
    args: [...(options.args || []), '--enable-automation', rasterArgument] });
  try {
    const cdp = await browser.newBrowserCDPSession();
    const { arguments: actualArguments } = await cdp.send('Browser.getBrowserCommandLine');
    assert.deepEqual(actualArguments.filter(arg => arg.startsWith(`${RASTER_SWITCH}=`)), [rasterArgument],
      'Chromium launch arguments drifted from the shared raster configuration');
    const { gpu } = await cdp.send('SystemInfo.getInfo');
    assert.equal(gpu.featureStatus.multiple_raster_threads, 'disabled_off',
      'Chromium did not enable the configured single raster worker');
    return { browser, cdp, environment: {
      implementationSha256, rasterThreads: RASTER_THREADS,
      rasterization: gpu.featureStatus.rasterization,
      gpuCompositing: gpu.featureStatus.gpu_compositing,
      devices: gpu.devices.map(({ vendorId, deviceId, vendorString, deviceString, driverVendor, driverVersion }) =>
        ({ vendorId, deviceId, vendorString, deviceString, driverVendor, driverVersion })),
    } };
  } catch (error) {
    await browser.close();
    throw error;
  }
}

module.exports = { launchBrowser };
