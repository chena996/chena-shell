import { describe, expect, it } from 'vitest';
import {
  basenameOf,
  detectEol,
  editorLanguageFor,
  formatBytes,
  languageLabel,
} from './languageSupport.js';

describe('editorLanguageFor', () => {
  it('maps supported extensions', () => {
    expect(editorLanguageFor('/srv/app/README.md')).toBe('markdown');
    expect(editorLanguageFor('/srv/app/deploy.sh')).toBe('shell');
    expect(editorLanguageFor('/srv/app/main.py')).toBe('python');
    expect(editorLanguageFor('/srv/app/values.yaml')).toBe('yaml');
    expect(editorLanguageFor('/srv/app/package.json')).toBe('json');
  });

  it('falls back to plaintext for unknown or extensionless files', () => {
    expect(editorLanguageFor('/srv/app/nginx.conf')).toBe('plaintext');
    expect(editorLanguageFor('/srv/app/LICENSE')).toBe('plaintext');
    expect(editorLanguageFor('')).toBe('plaintext');
  });
});

describe('detectEol', () => {
  it('detects CRLF and defaults to LF', () => {
    expect(detectEol('a\r\nb')).toBe('\r\n');
    expect(detectEol('a\nb')).toBe('\n');
    expect(detectEol('')).toBe('\n');
  });
});

describe('formatBytes', () => {
  it('formats byte sizes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1023)).toBe('1023 B');
    expect(formatBytes(2048)).toBe('2.0 KiB');
    expect(formatBytes(5 * 1024 * 1024)).toBe('5.00 MiB');
  });

  it('guards invalid input', () => {
    expect(formatBytes(-1)).toBe('0 B');
    expect(formatBytes('x')).toBe('0 B');
  });
});

describe('basenameOf', () => {
  it('returns the last path segment', () => {
    expect(basenameOf('/srv/app/main.py')).toBe('main.py');
    expect(basenameOf('/srv/app/')).toBe('app');
    expect(basenameOf('')).toBe('未命名');
  });
});

describe('languageLabel', () => {
  it('returns a display label', () => {
    expect(languageLabel('yaml')).toBe('YAML');
    expect(languageLabel('unknown')).toBe('纯文本');
  });
});
