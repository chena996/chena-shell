import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const executable = process.env.CHENA_MCP_HARNESS_EXE || join(root, 'src-tauri', 'target', 'debug', 'Chena-Shell.exe');
const token = `csh_${randomBytes(32).toString('base64url')}`;
const port = 18000 + Math.floor(Math.random() * 1000);
const endpoint = `http://127.0.0.1:${port}/mcp`;
const configDir = await mkdtemp(join(tmpdir(), 'chena-mcp-sdk-'));
const cleanEnv = Object.fromEntries(Object.entries(process.env).filter((entry) => entry[1] !== undefined));
const harnessEnv = {
  ...cleanEnv,
  CHENA_MCP_TOKEN: token,
  CHENA_MCP_PORT: String(port),
  CHENA_SHELL_TEST_CONFIG_DIR: configDir,
  CHENA_MCP_HARNESS_AUTO_APPROVE: '1',
};
let harness;
let httpClient;
let stdioClient;
let harnessOutput = '';

function payload(response) {
  if (response.structuredContent) return response.structuredContent;
  const text = response.content?.find((item) => item.type === 'text')?.text;
  return text ? JSON.parse(text) : {};
}

async function call(client, name, args = {}) {
  return payload(await client.callTool({ name, arguments: args }));
}

async function waitForHarness(child) {
  if (child.stdout.readableEncoding !== null) child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => { harnessOutput += chunk; });
  child.stderr.on('data', (chunk) => { harnessOutput += chunk; });
  const deadline = Date.now() + 20000;
  while (!harnessOutput.includes('CHENA_MCP_HARNESS_READY')) {
    if (child.exitCode !== null) throw new Error(`MCP harness 退出: ${harnessOutput}`);
    if (Date.now() > deadline) throw new Error(`MCP harness 启动超时: ${harnessOutput}`);
    await delay(50);
  }
}

async function approvedCall(client, name, args) {
  const pending = await call(client, name, args);
  assert.equal(pending.error, 'WAIT_CONFIRM', JSON.stringify(pending));
  assert.ok(pending.confirmation_id);
  await delay(200);
  return {
    confirmationId: pending.confirmation_id,
    result: await call(client, name, { ...args, confirmation_id: pending.confirmation_id }),
  };
}

const settingsWhitelist = [
  'mcp_enabled',
  'mcp_host',
  'mcp_port',
  'mcp_autostart',
  'attach_token_ttl_minutes',
  'idle_detach_minutes',
  'attach_token_visible',
  'theme',
  'log_ai_commands',
  'log_retention_days',
  'startup_auto_connect',
  'show_onboarding',
  'terminal_font_size',
  'copy_on_select',
  'confirm_multiline_paste',
  'links_enabled',
  'default_keepalive_secs',
  'auto_reconnect',
  'reconnect_max_attempts',
  'connect_timeout_secs',
  'terminal_log_default_enabled',
  'log_redaction_enabled',
].sort();

async function waitFor(check, description, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await delay(100);
  }
  throw new Error(`${description}${lastError ? `: ${lastError.message}` : ''}`);
}

async function sessionById(client, sessionId) {
  const listed = await call(client, 'session_list');
  return listed.sessions?.find((item) => item.config.id === sessionId);
}

async function attach(client, sessionId, transport, mode, sftp = false) {
  const args = {
    session_id: sessionId,
    reason: `official ${transport} SDK ${mode} capability smoke`,
    mode,
    ...(sftp ? { permissions: ['sftp'] } : {}),
  };
  const attached = await approvedCall(client, 'session_attach', args);
  assert.equal(attached.result.ok, true, JSON.stringify(attached.result));
  assert.equal(attached.result.mode, mode);
  assert.ok(attached.result.attach_token);
  if (mode === 'read') {
    assert.deepEqual([...attached.result.permissions].sort(), ['detach', 'read']);
  } else {
    assert.ok(attached.result.permissions.includes('exec'));
    assert.equal(attached.result.permissions.includes('sftp'), sftp);
  }
  return attached.result.attach_token;
}

async function detach(client, sessionId, attachToken, reason) {
  const result = await call(client, 'session_detach', {
    session_id: sessionId,
    attach_token: attachToken,
    reason,
  });
  assert.equal(result.ok, true, JSON.stringify(result));
}

async function exerciseNewCapabilities(client, sessionId, transport) {
  const firstReadToken = await attach(client, sessionId, transport, 'read');
  const denied = await call(client, 'session_exec', {
    session_id: sessionId,
    attach_token: firstReadToken,
    command: "printf 'READ_TOKEN_MUST_NOT_EXEC'",
    timeout_ms: 5000,
  });
  assert.notEqual(denied.ok, true, JSON.stringify(denied));
  assert.ok(denied.error?.includes('权限'), JSON.stringify(denied));

  const baseline = await waitFor(async () => {
    const replay = await call(client, 'session_read', {
      session_id: sessionId,
      attach_token: firstReadToken,
    });
    assert.equal(replay.ok, true, JSON.stringify(replay));
    return replay.chunks.some((chunk) => chunk.data.includes('CHENA_MCP_TERMINAL_REPLAY_'))
      ? replay
      : null;
  }, `${transport} 未读取到真实终端输出`);
  assert.ok(Number.isSafeInteger(baseline.next_seq));
  assert.ok(baseline.next_seq > 0, JSON.stringify(baseline));

  const settingsResult = await call(client, 'settings_get');
  assert.deepEqual(Object.keys(settingsResult), ['settings']);
  assert.deepEqual(Object.keys(settingsResult.settings).sort(), settingsWhitelist);
  const serializedSettings = JSON.stringify(settingsResult);
  for (const forbidden of [
    'mcp_credential_hash',
    'credential_hash',
    'default_policy',
    'global_blocklist',
    'confirm_list',
    'mcp_require_attach_approval',
    token,
    firstReadToken,
  ]) {
    assert.ok(!serializedSettings.includes(forbidden), `settings_get 泄露了 ${forbidden}`);
  }

  const readToken = firstReadToken;
  const writerToken = await attach(client, sessionId, transport, 'write', true);
  const replay = baseline;
  let previousSeq = 0;
  for (const chunk of replay.chunks) {
    assert.equal(chunk.generation, replay.generation, JSON.stringify(chunk));
    assert.ok(chunk.seq > previousSeq, JSON.stringify({ previousSeq, chunk }));
    assert.equal(typeof chunk.data, 'string');
    previousSeq = chunk.seq;
  }

  const testId = randomBytes(6).toString('hex');
  const remoteDir = `chena-mcp-sdk-${testId}`;
  const textPath = `${remoteDir}/terminal.txt`;
  const binaryPath = `${remoteDir}/bytes.bin`;
  const marker = `CHENA_MCP_SDK_${transport.toUpperCase()}_OK`;
  const executed = await call(client, 'session_exec', {
    session_id: sessionId,
    attach_token: writerToken,
    command: `mkdir '${remoteDir}' && printf '${marker}'`,
    timeout_ms: 5000,
  });
  assert.equal(executed.ok, true, JSON.stringify(executed));
  assert.equal(executed.exit_code, 0);
  assert.ok(executed.stdout.includes(marker));

  const writerReplay = await waitFor(async () => {
    const incremental = await call(client, 'session_read', {
      session_id: sessionId,
      attach_token: writerToken,
      after_seq: replay.next_seq,
    });
    assert.equal(incremental.ok, true, JSON.stringify(incremental));
    return incremental.chunks.some((chunk) => chunk.data.includes('CHENA_MCP_TERMINAL_REPLAY_'))
      ? incremental
      : null;
  }, `${transport} 未读取到递增的真实终端输出`);
  assert.ok(writerReplay.next_seq > replay.next_seq, JSON.stringify({ replay, writerReplay }));
  let writerPreviousSeq = replay.next_seq;
  for (const chunk of writerReplay.chunks) {
    assert.equal(chunk.generation, writerReplay.generation, JSON.stringify(chunk));
    assert.ok(chunk.seq > writerPreviousSeq, JSON.stringify({ writerPreviousSeq, chunk }));
    assert.equal(typeof chunk.data, 'string');
    writerPreviousSeq = chunk.seq;
  }
  console.log(
    `${transport} terminal replay passed: baseline_next_seq=${replay.next_seq}, incremental_next_seq=${writerReplay.next_seq}, incremental_chunks=${writerReplay.chunks.length}`,
  );

  const initialText = `CHENA_MCP_SFTP_V1 ${transport}\n第一行\n`;
  const updatedText = `CHENA_MCP_SFTP_V2 ${transport}\n覆盖完成\n`;
  const initialWrite = await call(client, 'sftp_write', {
    session_id: sessionId,
    attach_token: writerToken,
    path: textPath,
    content: initialText,
    encoding: 'text',
  });
  assert.equal(initialWrite.ok, true, JSON.stringify(initialWrite));
  assert.equal(initialWrite.overwritten, false);
  assert.equal(initialWrite.bytes_written, Buffer.byteLength(initialText));

  const binary = Buffer.from([0, 255, 254, 1, 2, 3, 65]);
  const binaryBase64 = binary.toString('base64');
  const binaryWrite = await call(client, 'sftp_write', {
    session_id: sessionId,
    attach_token: writerToken,
    path: binaryPath,
    content: binaryBase64,
    encoding: 'base64',
  });
  assert.equal(binaryWrite.ok, true, JSON.stringify(binaryWrite));
  assert.equal(binaryWrite.bytes_written, binary.length);

  const limitedList = await call(client, 'sftp_list', {
    session_id: sessionId,
    attach_token: writerToken,
    path: remoteDir,
    limit: 1,
  });
  assert.equal(limitedList.ok, true, JSON.stringify(limitedList));
  assert.equal(limitedList.entries.length, 1);
  assert.equal(limitedList.truncated, true);
  const fullList = await call(client, 'sftp_list', {
    session_id: sessionId,
    attach_token: writerToken,
    path: remoteDir,
    limit: 10,
  });
  assert.equal(fullList.ok, true, JSON.stringify(fullList));
  assert.deepEqual(fullList.entries.map((entry) => entry.name).sort(), ['bytes.bin', 'terminal.txt']);
  assert.equal(fullList.truncated, false);

  const textRead = await call(client, 'sftp_read', {
    session_id: sessionId,
    attach_token: writerToken,
    path: textPath,
    encoding: 'text',
  });
  assert.equal(textRead.ok, true, JSON.stringify(textRead));
  assert.equal(textRead.content, initialText);
  assert.equal(textRead.size, Buffer.byteLength(initialText));
  const binaryRead = await call(client, 'sftp_read', {
    session_id: sessionId,
    attach_token: writerToken,
    path: binaryPath,
    encoding: 'base64',
  });
  assert.equal(binaryRead.ok, true, JSON.stringify(binaryRead));
  assert.equal(binaryRead.content, binaryBase64);
  assert.ok(binary.equals(Buffer.from(binaryRead.content, 'base64')));

  const overwriteArgs = {
    session_id: sessionId,
    attach_token: writerToken,
    path: textPath,
    content: updatedText,
    encoding: 'text',
  };
  const pendingOverwrite = await call(client, 'sftp_write', overwriteArgs);
  assert.equal(pendingOverwrite.error, 'WAIT_CONFIRM', JSON.stringify(pendingOverwrite));
  assert.ok(pendingOverwrite.confirmation_id);
  const beforeApprovalRetry = await call(client, 'sftp_read', {
    session_id: sessionId,
    attach_token: writerToken,
    path: textPath,
    encoding: 'text',
  });
  assert.equal(beforeApprovalRetry.content, initialText, '覆盖写审批前目标已被修改');
  await delay(200);

  const wrongPath = await call(client, 'sftp_write', {
    ...overwriteArgs,
    path: binaryPath,
    content: 'WRONG_PATH_MUST_NOT_APPLY',
    confirmation_id: pendingOverwrite.confirmation_id,
  });
  assert.notEqual(wrongPath.ok, true, JSON.stringify(wrongPath));
  assert.ok(wrongPath.error?.includes('path does not match'), JSON.stringify(wrongPath));
  const wrongPathRead = await call(client, 'sftp_read', {
    session_id: sessionId,
    attach_token: writerToken,
    path: binaryPath,
    encoding: 'base64',
  });
  assert.equal(wrongPathRead.content, binaryBase64, '路径不匹配的审批修改了其他文件');

  const overwritten = await call(client, 'sftp_write', {
    ...overwriteArgs,
    confirmation_id: pendingOverwrite.confirmation_id,
  });
  assert.equal(overwritten.ok, true, JSON.stringify(overwritten));
  assert.equal(overwritten.overwritten, true);
  assert.equal(overwritten.bytes_written, Buffer.byteLength(updatedText));
  const duplicate = await call(client, 'sftp_write', {
    ...overwriteArgs,
    content: 'DUPLICATE_CONFIRMATION_MUST_NOT_APPLY',
    confirmation_id: pendingOverwrite.confirmation_id,
  });
  assert.notEqual(duplicate.ok, true, JSON.stringify(duplicate));
  assert.ok(duplicate.error?.includes('消费'), JSON.stringify(duplicate));
  const finalRead = await call(client, 'sftp_read', {
    session_id: sessionId,
    attach_token: writerToken,
    path: textPath,
    encoding: 'text',
  });
  assert.equal(finalRead.content, updatedText, '重复消费审批修改了目标文件');

  const cleanup = await call(client, 'session_exec', {
    session_id: sessionId,
    attach_token: writerToken,
    command: `rm -f '${textPath}' '${binaryPath}' && rmdir '${remoteDir}'`,
    timeout_ms: 5000,
  });
  assert.equal(cleanup.ok, true, JSON.stringify(cleanup));
  assert.equal(cleanup.exit_code, 0, JSON.stringify(cleanup));
  console.log(`${transport} new-capability smoke passed: read/write attach, replay, settings whitelist, real SFTP and approval binding.`);
  return { readToken, writerToken };
}

try {
  harness = spawn(executable, ['--mcp-harness'], {
    cwd: root,
    env: harnessEnv,
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });
  await waitForHarness(harness);

  const unauthorized = await fetch(endpoint, {
    method: 'POST',
    headers: { Accept: 'application/json, text/event-stream', 'Content-Type': 'application/json' },
    body: '{}',
  });
  assert.equal(unauthorized.status, 401);
  const remoteOrigin = await fetch(endpoint, {
    method: 'POST',
    headers: {
      Accept: 'application/json, text/event-stream',
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
      Origin: 'https://example.com',
    },
    body: '{}',
  });
  assert.equal(remoteOrigin.status, 403);

  httpClient = new Client({ name: 'official-http-sdk-smoke', version: '1.0.0' }, { capabilities: {} });
  const httpTransport = new StreamableHTTPClientTransport(new URL(endpoint), {
    requestInit: { headers: { Authorization: `Bearer ${token}`, Origin: 'http://localhost' } },
  });
  await httpClient.connect(httpTransport);
  const tools = await httpClient.listTools();
  for (const name of [
    'session_exec',
    'session_attach',
    'session_read',
    'sftp_list',
    'sftp_read',
    'sftp_write',
    'settings_get',
  ]) {
    assert.ok(tools.tools.some((tool) => tool.name === name), `缺少 MCP 工具 ${name}`);
  }
  assert.ok(!tools.tools.some((tool) => tool.name === 'settings_set'));
  const attachTool = tools.tools.find((tool) => tool.name === 'session_attach');
  assert.equal(attachTool.inputSchema.properties.mode.default, 'write');

  const listed = await call(httpClient, 'session_list');
  const session = listed.sessions?.find((item) => item.config.id === 'mcp-real-openssh');
  assert.ok(session, 'MCP session_list 未返回真实 OpenSSH 会话');
  assert.equal(session.status.connected, true);
  const serializedList = JSON.stringify(listed);
  assert.ok(!serializedList.includes('password_ref') && !serializedList.includes('passphrase_ref'));
  assert.ok(!serializedList.includes('attach_token'));

  const { readToken, writerToken: attachToken } = await exerciseNewCapabilities(
    httpClient,
    session.config.id,
    'HTTP',
  );
  const attachedList = JSON.stringify(await call(httpClient, 'session_list'));
  assert.ok(!attachedList.includes(attachToken), 'session_list 泄露了 writer attach token');
  assert.ok(!attachedList.includes(readToken), 'session_list 泄露了 read attach token');

  const executed = await call(httpClient, 'session_exec', {
    session_id: session.config.id,
    attach_token: attachToken,
    command: "printf 'CHENA_MCP_SDK_OK'",
    timeout_ms: 5000,
  });
  assert.equal(executed.ok, true, JSON.stringify(executed));
  assert.equal(executed.exit_code, 0);
  assert.ok(executed.stdout.includes('CHENA_MCP_SDK_OK'));

  const blocked = await call(httpClient, 'session_exec', {
    session_id: session.config.id,
    attach_token: attachToken,
    command: 'rm -rf /',
    timeout_ms: 5000,
  });
  assert.equal(blocked.error, 'BLOCKED', JSON.stringify(blocked));

  const confirmedArgs = {
    session_id: session.config.id,
    attach_token: attachToken,
    command: 'systemctl restart chena-sdk-demo',
    timeout_ms: 5000,
  };
  const confirmed = await approvedCall(httpClient, 'session_exec', confirmedArgs);
  assert.equal(confirmed.result.ok, true, JSON.stringify(confirmed.result));
  const duplicate = await call(httpClient, 'session_exec', {
    ...confirmedArgs,
    confirmation_id: confirmed.confirmationId,
  });
  assert.equal(duplicate.ok, false);
  assert.ok(duplicate.error.includes('消费') || duplicate.error.includes('重复'), JSON.stringify(duplicate));

  const timedOut = await call(httpClient, 'session_exec', {
    session_id: session.config.id,
    attach_token: attachToken,
    command: 'sleep 2',
    timeout_ms: 1000,
  });
  assert.equal(timedOut.error, 'TIMEOUT', JSON.stringify(timedOut));
  assert.equal(timedOut.timed_out, true);

  await detach(httpClient, session.config.id, attachToken, 'HTTP SDK writer smoke complete');
  await detach(httpClient, session.config.id, readToken, 'HTTP SDK read smoke complete');

  const stdioTransport = new StdioClientTransport({
    command: executable,
    args: ['--mcp-stdio'],
    cwd: root,
    env: { ...cleanEnv, CHENA_MCP_TOKEN: token, CHENA_MCP_URL: endpoint },
    stderr: 'pipe',
  });
  stdioTransport.stderr?.on('data', () => {});
  stdioClient = new Client({ name: 'official-stdio-sdk-smoke', version: '1.0.0' }, { capabilities: {} });
  await stdioClient.connect(stdioTransport);
  const stdioTools = await stdioClient.listTools();
  assert.ok(stdioTools.tools.some((tool) => tool.name === 'session_exec'));
  assert.ok(stdioTools.tools.some((tool) => tool.name === 'session_read'));
  assert.ok(stdioTools.tools.some((tool) => tool.name === 'sftp_write'));
  const stdioSessions = await call(stdioClient, 'session_list');
  assert.ok(stdioSessions.sessions?.some((item) => item.config.id === session.config.id));
  const stdioTokens = await exerciseNewCapabilities(stdioClient, session.config.id, 'stdio');
  await detach(stdioClient, session.config.id, stdioTokens.writerToken, 'stdio SDK writer smoke complete');
  await detach(stdioClient, session.config.id, stdioTokens.readToken, 'stdio SDK read smoke complete');

  console.log('MCP SDK smoke passed: official HTTP + stdio clients, legacy exec security, read/write attach, real terminal replay, settings whitelist, real SFTP, path-bound single-use overwrite approval.');
} catch (error) {
  if (harnessOutput) console.error(harnessOutput);
  throw error;
} finally {
  await Promise.allSettled([httpClient?.close(), stdioClient?.close()].filter(Boolean));
  if (harness && harness.exitCode === null) {
    const exited = new Promise((resolve) => harness.once('exit', resolve));
    harness.kill();
    await Promise.race([exited, delay(3000)]);
  }
  await rm(configDir, { recursive: true, force: true });
}
