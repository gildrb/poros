import { test, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

const remote = process.env.POROS_TEST_SSH;
const root = process.env.POROS_TEST_ROOT;
if (!process.env.POROS_TEST_URL || !root) {
  throw new Error('Set POROS_TEST_URL and POROS_TEST_ROOT; optionally POROS_TEST_SSH.');
}
const source = join(root, 'message.ts');
function writeMessage(value: string) {
  const contents = `export const message = ${JSON.stringify(value)};\n`;
  if (remote) {
    execFileSync('ssh', ['-o', 'BatchMode=yes', remote, 'python3', '-c',
      `"import pathlib,sys; pathlib.Path(sys.argv[1]).write_text(sys.stdin.read())"`,
      `'${source.replaceAll("'", "'\\''")}'`], { input: contents, timeout: 10_000 });
  } else {
    writeFileSync(source, contents);
  }
}

test('private HTTPS loads and Vite updates without reloading', async ({ page }) => {
  writeMessage('poros-before');
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  const socket = page.waitForEvent('websocket');
  await page.goto('/');
  await expect(page.locator('#app')).toHaveText('poros-before');
  const websocket = await socket;
  expect(websocket.url()).toMatch(/^wss:/);
  expect(new URL(websocket.url()).host).toBe(new URL(process.env.POROS_TEST_URL!).host);
  expect(await page.evaluate(() => window.isSecureContext)).toBe(true);
  await page.evaluate(() => { (window as any).__porosDocument = 'retained'; });
  try {
    writeMessage('poros-after');
    await expect(page.locator('#app')).toHaveText('poros-after', { timeout: 15_000 });
    expect(await page.evaluate(() => (window as any).__porosDocument)).toBe('retained');
    expect(errors).toEqual([]);
  } finally {
    writeMessage('poros-before');
  }
});
