import { copyFileSync, existsSync, mkdirSync, unlinkSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const tauriRoot = resolve(projectRoot, 'src-tauri');

function rustHostTriple() {
  const version = execFileSync('rustc', ['-vV'], {
    cwd: projectRoot,
    encoding: 'utf8',
  });
  const host = version.match(/^host:\s*(\S+)$/m)?.[1];
  if (!host) {
    throw new Error('Could not determine Rust host target triple from rustc -vV');
  }
  return host;
}

const targetTriple =
  process.env.TAURI_ENV_TARGET_TRIPLE?.trim() || rustHostTriple();
if (!/^[A-Za-z0-9_]+-[A-Za-z0-9_]+-[A-Za-z0-9_]+(?:-[A-Za-z0-9_]+)*$/.test(targetTriple)) {
  throw new Error('Invalid target triple: ' + targetTriple);
}

const extension = targetTriple.includes('windows') ? '.exe' : '';
const output = resolve(
  tauriRoot,
  'target',
  targetTriple,
  'release',
  'mcp_server' + extension,
);
const sidecarDirectory = resolve(tauriRoot, 'binaries');
const sidecar = resolve(
  sidecarDirectory,
  'omnitun-mcp-' + targetTriple + extension,
);

mkdirSync(sidecarDirectory, { recursive: true });

if (process.argv.includes("--placeholder")) {
  // Rust CI jobs only need this path for tauri-build validation. The zero-byte
  // marker is never executed or shipped; release builds create the real binary.
  if (!existsSync(sidecar)) writeFileSync(sidecar, Buffer.alloc(0));
  console.log("Prepared MCP sidecar placeholder: " + sidecar);
  process.exit(0);
}
// Tauri's build script validates externalBin paths while compiling the MCP
// crate itself. Create a temporary marker to break that bootstrap cycle, then
// always replace it with the real binary before the Tauri bundle step starts.
const createdPlaceholder = !existsSync(sidecar);
if (createdPlaceholder) writeFileSync(sidecar, Buffer.alloc(0));
console.log('Building OmniTun MCP server for ' + targetTriple + '...');
try {
  execFileSync(
    'cargo',
    [
      'build',
      '--release',
      '--no-default-features',
      '--features',
      'mcp-server',
      '--bin',
      'mcp_server',
      '--target',
      targetTriple,
      '--manifest-path',
      'src-tauri/Cargo.toml',
    ],
    { cwd: projectRoot, stdio: 'inherit' },
  );
} catch (error) {
  if (createdPlaceholder && existsSync(sidecar)) unlinkSync(sidecar);
  throw error;
}

if (!existsSync(output)) {
  throw new Error('Cargo reported success but MCP executable was not found: ' + output);
}

copyFileSync(output, sidecar);
console.log('Prepared Tauri sidecar: ' + sidecar);
