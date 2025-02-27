// Common utility functions used throughout the tests

'use strict';

const sleep = require('sleep-promise');

async function waitUntil(test, timeout, name) {
  let delay = 1;

  while (true) {
    let done = await test();
    if (done) {
      return;
    }
    if (timeout <= 0) {
      throw new Error('Timed out waiting for ' + name);
    }
    await sleep(delay);
    timeout -= delay;
    delay *= 2;
    if (delay > 100) {
      delay = 100;
    }
  }
}

// Check that the test callback which should return a future fails with
// given grpc error code.
async function shouldFailWith(code, test) {
  try {
    await test();
  } catch (err) {
    if (err.code != code) {
      throw err;
    }
    return;
  }
  throw new Error('Expected error');
}

module.exports = {
  shouldFailWith,
  waitUntil,
};
