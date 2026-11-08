import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');
const readJson = (file) => JSON.parse(readFileSync(resolve(root, file), 'utf8'));
const packageVersion = readJson('package.json').version;
const tauriVersion = readJson('src-tauri/tauri.conf.json').version;
const cargoToml = readFileSync(resolve(root, 'src-tauri/Cargo.toml'), 'utf8');
const cargoVersion = cargoToml.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];

if (!cargoVersion) throw new Error('Could not read the root Cargo package version');
const versions = { packageVersion, tauriVersion, cargoVersion };
if (new Set(Object.values(versions)).size !== 1) {
  throw new Error('Application version mismatch: ' + JSON.stringify(versions));
}
console.log('Release metadata is consistent:', packageVersion);
