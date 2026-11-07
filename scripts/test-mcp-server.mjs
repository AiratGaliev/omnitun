import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const rustInfo = spawnSync('rustc', ['-vV'], { cwd: projectRoot, encoding: 'utf8' });
if (rustInfo.status !== 0) throw new Error(rustInfo.stderr || 'rustc -vV failed');
const targetTriple =
  process.env.TAURI_ENV_TARGET_TRIPLE ||
  rustInfo.stdout.match(/^host:\s*(\S+)$/m)?.[1];
if (!targetTriple) throw new Error('Cannot detect Rust target triple');

const extension = targetTriple.includes('windows') ? '.exe' : '';
const executable = resolve(
  projectRoot,
  'src-tauri',
  'binaries',
  'omnitun-mcp-' + targetTriple + extension,
);
if (!existsSync(executable)) {
  throw new Error('MCP sidecar not found. Run npm run prepare:mcp-sidecar first: ' + executable);
}

const temporaryRoot = mkdtempSync(join(tmpdir(), 'omnitun-mcp-functional-'));
const dataDir = join(temporaryRoot, 'com.omnitun.desktop');
const configDir = join(dataDir, 'config');
mkdirSync(configDir, { recursive: true });

const externalProcess = spawn(
  process.execPath,
  ['-e', 'setInterval(() => {}, 1000)'],
  { stdio: 'ignore', windowsHide: true },
);
let externalProcessWasKilled = false;

const externalProxyPort = 17555;
const externalClashPort = 17556;
writeFileSync(
  join(configDir, 'config.json'),
  JSON.stringify({
    log: { level: 'warn' },
    inbounds: [{
      type: 'mixed',
      tag: 'mixed-in',
      listen: '127.0.0.1',
      listen_port: externalProxyPort,
      set_system_proxy: false,
    }],
    outbounds: [{ type: 'block', tag: 'block' }],
    experimental: {
      clash_api: { external_controller: '127.0.0.1:' + externalClashPort },
    },
  }, null, 2),
);

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function responsesFrom(stdout) {
  const responseMap = new Map();
  for (const line of stdout.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed.startsWith('{')) continue;
    try {
      const item = JSON.parse(trimmed);
      if (item.id !== undefined) responseMap.set(item.id, item);
    } catch {
      // Ignore startup diagnostics; protocol responses are validated by ID below.
    }
  }
  return responseMap;
}

function initializeRequests(clientName) {
  return [
    {
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: {
        protocolVersion: '2025-03-26',
        capabilities: {},
        clientInfo: { name: clientName, version: '1.0.0' },
      },
    },
    { jsonrpc: '2.0', method: 'notifications/initialized' },
  ];
}

function runMcp(requestList, env, phase) {
  const result = spawnSync(executable, [], {
    cwd: projectRoot,
    env,
    input: requestList.map((item) => JSON.stringify(item)).join('\n') + '\n',
    encoding: 'utf8',
    timeout: 60000,
    maxBuffer: 32 * 1024 * 1024,
    windowsHide: true,
  });
  if (result.error) throw result.error;
  assert(result.status === 0, phase + ' MCP exit code was ' + result.status + ': ' + result.stderr);
  assert(!result.stdout.includes('mcp-secret-token-must-not-leak'),
    phase + ' leaked the IPinfo token in MCP output');
  return responsesFrom(result.stdout);
}

try {
  await once(externalProcess, 'spawn');
  writeFileSync(join(dataDir, 'singbox.pid'), String(externalProcess.pid));

  const env = {
    ...process.env,
    APPDATA: temporaryRoot,
    RUST_LOG: 'error',
  };

  // Discovery and independent read/write operations.
  const discoveryRequests = [
    ...initializeRequests('omnitun-functional-qa'),
    { jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} },
    { jsonrpc: '2.0', id: 3, method: 'resources/list', params: {} },
    { jsonrpc: '2.0', id: 4, method: 'tools/call', params: { name: 'get_status', arguments: {} } },
    { jsonrpc: '2.0', id: 5, method: 'resources/read', params: { uri: 'omnitun://status' } },
    {
      jsonrpc: '2.0',
      id: 7,
      method: 'tools/call',
      params: {
        name: 'update_settings',
        arguments: {
          language: 'ru',
          proxy_port: 17666,
          routing_mode: 'local-proxy',
          custom_dns: false,
          dns_address: '1.1.1.1',
          auto_test_interval: 15,
          test_url: 'https://example.com/health',
          test_use_tls: true,
          ipinfo_token: 'mcp-secret-token-must-not-leak',
        },
      },
    },
    { jsonrpc: '2.0', id: 10, method: 'tools/call', params: { name: 'update_settings', arguments: { proxy_port: 70000 } } },
    { jsonrpc: '2.0', id: 11, method: 'tools/call', params: { name: 'update_settings', arguments: { unsupported_setting: true } } },
    { jsonrpc: '2.0', id: 12, method: 'tools/call', params: { name: 'clear_cache', arguments: {} } },
    { jsonrpc: '2.0', id: 13, method: 'tools/call', params: { name: 'list_profiles', arguments: {} } },
    {
      jsonrpc: '2.0',
      id: 14,
      method: 'tools/call',
      params: { name: 'import_subscription_content', arguments: { name: 'qa-invalid', content: 'not-a-valid-subscription' } },
    },
    { jsonrpc: '2.0', id: 15, method: 'tools/call', params: { name: '__unknown_qa_tool', arguments: {} } },
    { jsonrpc: '2.0', id: 16, method: 'tools/call', params: { name: 'check_proxy_health', arguments: {} } },
  ];

  const resourceUris = [
    'omnitun://logs/recent',
    'omnitun://profiles',
    'omnitun://cache',
    'omnitun://geoip/status',
    'omnitun://singbox/stderr',
    'omnitun://singbox/config',
  ];
  resourceUris.forEach((uri, index) => {
    discoveryRequests.push({ jsonrpc: '2.0', id: 20 + index, method: 'resources/read', params: { uri } });
  });

  const map = runMcp(discoveryRequests, env, 'discovery and update');
  assert(map.get(1)?.result?.serverInfo?.name === 'OmniTun MCP', 'MCP initialize failed');

  const tools = map.get(2)?.result?.tools ?? [];
  assert(tools.length >= 27, 'tools/list returned only ' + tools.length + ' tools');
  const toolMap = new Map(tools.map((tool) => [tool.name, tool]));
  assert(toolMap.has('start_proxy') && toolMap.has('stop_proxy'), 'proxy lifecycle tools missing');
  assert(toolMap.has('update_settings') && toolMap.has('clear_cache'), 'settings/cache tools missing');
  const settingsProperties = toolMap.get('update_settings')?.inputSchema?.properties ?? {};
  for (const key of ['language', 'allowed_countries', 'blocked_protocols', 'custom_dns', 'ipinfo_token']) {
    assert(settingsProperties[key], 'update_settings schema is missing ' + key);
  }
  assert(!settingsProperties.accept_terms, 'MCP must not silently accept terms on behalf of the user');

  const resources = map.get(3)?.result?.resources ?? [];
  assert(resources.length >= 8, 'resources/list returned only ' + resources.length + ' resources');
  for (const uri of ['omnitun://status', 'omnitun://settings', ...resourceUris]) {
    assert(resources.some((resource) => resource.uri === uri), 'Missing MCP resource ' + uri);
  }

  const status = JSON.parse(map.get(4)?.result?.content?.[0]?.text ?? '{}');
  const statusResource = JSON.parse(map.get(5)?.result?.contents?.[0]?.text ?? '{}');
  assert(status.connected === true, 'MCP did not detect the external OmniTun PID file');
  assert(status.pid === externalProcess.pid, 'MCP status PID does not match external process');
  assert(status.proxy_port === externalProxyPort, 'MCP status did not read the actual proxy port');
  assert(status.clash_api_port === externalClashPort, 'MCP status did not read Clash API port');
  assert(status.routing_mode === 'local-proxy', 'MCP status did not detect routing mode');
  assert(statusResource.pid === status.pid && statusResource.proxy_port === status.proxy_port,
    'get_status and omnitun://status disagree');

  const updateText = map.get(7)?.result?.content?.[0]?.text ?? '';
  assert(updateText.includes('"language": "ru"'), 'update_settings did not persist language');
  assert(updateText.includes('"proxyPort": 17666'), 'update_settings did not persist proxy_port');
  assert(map.get(10)?.error?.code === -32602, 'Invalid proxy port was not rejected');
  assert(map.get(11)?.error?.code === -32602, 'Unknown settings field was not rejected');
  assert((map.get(12)?.result?.content?.[0]?.text ?? '').toLowerCase().includes('cleared'),
    'clear_cache did not report success');
  assert(map.has(13), 'list_profiles did not respond');
  assert(map.has(14), 'Invalid subscription import did not respond');
  assert(map.get(15)?.error?.code === -32601, 'Unknown tool was not rejected with METHOD_NOT_FOUND');
  assert(map.has(16), 'check_proxy_health did not respond');
  for (let i = 0; i < resourceUris.length; i++) {
    assert(map.has(20 + i), 'Failed to read MCP resource ' + resourceUris[i]);
  }

  // A fresh process reads the settings only after update_settings has completed.
  const settingsMap = runMcp([
    ...initializeRequests('omnitun-settings-qa'),
    { jsonrpc: '2.0', id: 8, method: 'tools/call', params: { name: 'get_settings', arguments: {} } },
    { jsonrpc: '2.0', id: 9, method: 'resources/read', params: { uri: 'omnitun://settings' } },
  ], env, 'settings persistence');

  const settings = JSON.parse(settingsMap.get(8)?.result?.content?.[0]?.text ?? '{}');
  assert(settings.language === 'ru' && settings.proxyPort === 17666,
    'get_settings did not return persisted values after update_settings');
  assert(settings.ipinfoTokenConfigured === true, 'get_settings did not report token configuration');
  assert(!Object.hasOwn(settings, 'ipinfoToken'), 'get_settings returned the secret field');
  const settingsResourceText = settingsMap.get(9)?.result?.contents?.[0]?.text ?? '';
  assert(!settingsResourceText.includes('mcp-secret-token-must-not-leak'),
    'Settings resource leaked the IPinfo token');

  // Stop a process that was started outside this MCP instance, then verify state
  // from a separate client process after the stop request has completed.
  const stopped = runMcp([
    ...initializeRequests('omnitun-stop-qa'),
    { jsonrpc: '2.0', id: 100, method: 'tools/call', params: { name: 'stop_proxy', arguments: {} } },
  ], env, 'cross-process stop');
  assert(stopped.get(100)?.result, 'MCP stop_proxy did not return a result');

  const afterStop = runMcp([
    ...initializeRequests('omnitun-status-after-stop-qa'),
    { jsonrpc: '2.0', id: 101, method: 'tools/call', params: { name: 'get_status', arguments: {} } },
  ], env, 'status after stop');
  const stoppedStatus = JSON.parse(afterStop.get(101)?.result?.content?.[0]?.text ?? '{}');
  assert(stoppedStatus.connected === false, 'MCP stop did not clear proxy status');
  assert(!existsSync(join(dataDir, 'singbox.pid')), 'MCP stop did not remove the shared PID file');

  try {
    process.kill(externalProcess.pid, 0);
  } catch {
    externalProcessWasKilled = true;
  }
  assert(externalProcessWasKilled, 'MCP stop did not terminate the external process');

  // Exercise a real ProcessManager start without touching a real VPN binary.
  // A tiny local executable ignores sing-box arguments and only stays alive;
  // the MCP server still uses the real Windows JobObject + shared PID logic.
  const binaryDirectory = join(dataDir, 'binaries');
  mkdirSync(binaryDirectory, { recursive: true });
  const fakeSource = join(temporaryRoot, 'fake-singbox.rs');
  const fakeBinary = join(binaryDirectory, 'sing-box.exe');
  writeFileSync(
    fakeSource,
    'fn main() { std::thread::sleep(std::time::Duration::from_secs(180)); }',
  );
  const fakeBuild = spawnSync('rustc', [fakeSource, '--edition=2021', '-o', fakeBinary], {
    cwd: projectRoot,
    encoding: 'utf8',
    timeout: 30000,
    windowsHide: true,
  });
  if (fakeBuild.error) throw fakeBuild.error;
  assert(fakeBuild.status === 0, 'Could not build safe fake sing-box process: ' + fakeBuild.stderr);

  const firstMcp = spawn(executable, [], {
    cwd: projectRoot,
    env,
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  });
  const outputLines = createInterface({ input: firstMcp.stdout });
  const waiting = new Map();
  let serverStderr = '';
  firstMcp.stderr.setEncoding('utf8');
  firstMcp.stderr.on('data', (chunk) => { serverStderr += chunk; });
  outputLines.on('line', (line) => {
    if (!line.trim().startsWith('{')) return;
    try {
      const response = JSON.parse(line);
      if (response.id !== undefined && waiting.has(response.id)) {
        const callbacks = waiting.get(response.id);
        waiting.delete(response.id);
        if (response.error) callbacks.reject(new Error(JSON.stringify(response.error)));
        else callbacks.resolve(response);
      }
    } catch {
      // Ignore non-JSON diagnostic lines; the protocol reply is validated by ID.
    }
  });
  const requestLongLived = (id, method, params = {}) => new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      waiting.delete(id);
      reject(new Error('Timed out waiting for MCP response ID ' + id));
    }, 30000);
    waiting.set(id, {
      resolve: (value) => { clearTimeout(timeout); resolve(value); },
      reject: (error) => { clearTimeout(timeout); reject(error); },
    });
    firstMcp.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });

  try {
    await requestLongLived(1, 'initialize', {
      protocolVersion: '2025-03-26',
      capabilities: {},
      clientInfo: { name: 'omnitun-live-process-qa', version: '1.0.0' },
    });
    firstMcp.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
    const activated = await requestLongLived(2, 'tools/call', {
      name: 'activate_kill_switch',
      arguments: {},
    });
    const activationText = activated.result?.content?.[0]?.text ?? '';
    const startedPid = Number(activationText.match(/PID\s+(\d+)/i)?.[1] || 0);
    assert(startedPid > 0, 'activate_kill_switch did not return the started process PID: ' + activationText);
    const pidFile = join(dataDir, 'singbox.pid');
    assert(existsSync(pidFile), 'ProcessManager.start_headless did not write singbox.pid');
    assert(Number((await import('node:fs')).readFileSync(pidFile, 'utf8').trim()) === startedPid,
      'Shared PID file does not match the process started by MCP');

    const detected = runMcp([
      ...initializeRequests('omnitun-live-detect-qa'),
      { jsonrpc: '2.0', id: 110, method: 'tools/call', params: { name: 'get_status', arguments: {} } },
    ], env, 'detect MCP-started process');
    const detectedStatus = JSON.parse(detected.get(110)?.result?.content?.[0]?.text ?? '{}');
    assert(detectedStatus.connected === true && detectedStatus.pid === startedPid,
      'Second MCP process did not discover the process started by the first MCP process');
    assert(detectedStatus.proxy_port === 17666 && detectedStatus.routing_mode === 'local-proxy',
      'Second MCP process did not read the active kill-switch config');

    const stoppedBySecond = runMcp([
      ...initializeRequests('omnitun-live-stop-qa'),
      { jsonrpc: '2.0', id: 111, method: 'tools/call', params: { name: 'stop_proxy', arguments: {} } },
    ], env, 'stop MCP-started process from second client');
    assert(stoppedBySecond.get(111)?.result, 'Second MCP process could not stop the first MCP process');
    assert(!existsSync(pidFile), 'Cross-process stop did not remove the shared PID file');
  } finally {
    const exited = once(firstMcp, 'exit');
    if (firstMcp.exitCode === null) firstMcp.stdin.end();
    const [exitCode] = await Promise.race([
      exited,
      new Promise((_, reject) => setTimeout(() => reject(new Error('Long-lived MCP process did not exit: ' + serverStderr)), 10000)),
    ]);
    assert(exitCode === 0, 'Long-lived MCP server exited unexpectedly: ' + serverStderr);
    rmSync(join(dataDir, 'singbox.stop'), { force: true });
  }

  console.log('MCP functional QA passed.');
  console.log('Verified: JSON-RPC initialize, ' + tools.length + ' tools, ' + resources.length +
    ' resources, all resource reads, settings persistence/validation, secret redaction, cache clear, ' +
    'profile read, invalid import, unknown tool, shared-process discovery, status consistency, and cross-process stop, including MCP-started process discovery and stop.');
} finally {
  if (externalProcess.exitCode === null && !externalProcess.killed) {
    try { externalProcess.kill(); } catch {}
  }
  rmSync(temporaryRoot, { recursive: true, force: true });
}
