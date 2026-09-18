// Pure helpers for Media over QUIC playback. Kept dependency-free and
// separate from MoqPlayer.tsx so they can be unit tested without a browser
// (WebTransport, WebCodecs, custom elements).

/**
 * Decodes a lowercase hex string — the shape `GET /moq/fingerprint` returns
 * (STATUS.md "Batch 5") — into raw bytes, the shape WebTransport's
 * `serverCertificateHashes[].value` expects.
 *
 * `@moq/net`'s `CertificateHash.value` already accepts a hex string directly
 * and decodes it the same way, but we parse it ourselves so a malformed
 * fingerprint fails loudly here (as an error state) instead of silently
 * no-op'ing the certificate pin, and so this logic has its own unit test.
 */
export function hexFingerprintToBytes(hex: string): Uint8Array<ArrayBuffer> {
  const clean = hex.trim().toLowerCase();
  if (clean.length === 0 || clean.length % 2 !== 0 || !/^[0-9a-f]+$/.test(clean)) {
    throw new Error(`invalid fingerprint hex: "${hex}"`);
  }
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/**
 * Builds the browser's MoQ connection URL: the page's `?token=` is forwarded
 * as `?jwt=` on the relay URL, per the server contract (STATUS.md
 * "Batch 5" — "Auth token, when needed, goes in the connection URL query as
 * `?jwt=<token>`"). Any existing query parameters on `relayUrl` are kept.
 */
export function buildMoqUrl(relayUrl: string, token?: string): URL {
  const url = new URL(relayUrl);
  if (token) url.searchParams.set('jwt', token);
  return url;
}
