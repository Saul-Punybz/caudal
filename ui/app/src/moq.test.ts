import { describe, expect, it } from 'vitest';
import { buildMoqUrl, hexFingerprintToBytes } from './moq';

describe('hexFingerprintToBytes', () => {
  it('decodes a hex string into raw bytes', () => {
    expect(Array.from(hexFingerprintToBytes('00ff10'))).toEqual([0, 255, 16]);
  });

  it('is case-insensitive', () => {
    expect(Array.from(hexFingerprintToBytes('AaBbCc'))).toEqual(Array.from(hexFingerprintToBytes('aabbcc')));
  });

  it('round-trips a full sha-256 length fingerprint (32 bytes)', () => {
    const hex = 'a1'.repeat(32);
    const bytes = hexFingerprintToBytes(hex);
    expect(bytes.length).toBe(32);
    expect(bytes[0]).toBe(0xa1);
  });

  it('throws on odd-length input', () => {
    expect(() => hexFingerprintToBytes('abc')).toThrow(/invalid fingerprint hex/);
  });

  it('throws on non-hex characters', () => {
    expect(() => hexFingerprintToBytes('zz')).toThrow(/invalid fingerprint hex/);
  });

  it('throws on empty input', () => {
    expect(() => hexFingerprintToBytes('')).toThrow(/invalid fingerprint hex/);
  });
});

describe('buildMoqUrl', () => {
  it('appends the token as ?jwt= when present', () => {
    const url = buildMoqUrl('https://example.com:4443', 'abc123');
    expect(url.toString()).toBe('https://example.com:4443/?jwt=abc123');
  });

  it('leaves the URL unchanged when there is no token', () => {
    const url = buildMoqUrl('https://example.com:4443');
    expect(url.searchParams.has('jwt')).toBe(false);
    expect(url.toString()).toBe('https://example.com:4443/');
  });

  it('preserves existing query parameters', () => {
    const url = buildMoqUrl('https://example.com:4443/anon?foo=bar', 'tok');
    expect(url.searchParams.get('foo')).toBe('bar');
    expect(url.searchParams.get('jwt')).toBe('tok');
  });

  it('percent-encodes a token with special characters', () => {
    const url = buildMoqUrl('https://example.com:4443', 'a b/c');
    expect(url.searchParams.get('jwt')).toBe('a b/c');
    expect(url.toString()).toContain('jwt=a+b%2Fc');
  });
});
