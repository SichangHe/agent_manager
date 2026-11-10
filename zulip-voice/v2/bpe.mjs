/**
 * Sentencepiece BPE tokenizer for the keyword spotter (so the browser can turn any phrase, e.g. an agent's name, into the pieces sherpa expects).
 * Reads the model's protobuf (`bpe.model`): repeated field 1 = {1: piece, 2: score(float), 3: type}. Encoding is the standard BPE: start from characters, repeatedly merge
 * the adjacent pair whose merged piece has the best score.
 */
export function parseBpe(bytes) {
  const pieces = new Map();
  let i = 0;
  const varint = () => { let n = 0, shift = 0, b; do { b = bytes[i++]; n += (b & 0x7f) * 2 ** shift; shift += 7; } while (b & 0x80); return n; };
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  while (i < bytes.length) {
    const tag = varint(), field = tag >> 3, wire = tag & 7;
    if (wire === 2) {
      const len = varint(), end = i + len;
      if (field === 1) {   // one SentencePiece
        let piece = '', score = 0, type = 1;
        while (i < end) {
          const t = varint(), f = t >> 3, w = t & 7;
          if (w === 2) { const l = varint(); if (f === 1) piece = new TextDecoder().decode(bytes.subarray(i, i + l)); i += l; }
          else if (w === 5) { if (f === 2) score = dv.getFloat32(i, true); i += 4; }
          else if (w === 0) { const v = varint(); if (f === 3) type = v; }
          else if (w === 1) i += 8;
        }
        if (type === 1) pieces.set(piece, score);   // normal pieces only
      } else i = end;
    } else if (wire === 0) varint(); else if (wire === 5) i += 4; else if (wire === 1) i += 8;
  }
  return pieces;
}
/** Pieces of `text` (upper-cased words): the segmentation with the best total score (unigram model Viterbi), e.g. "tomato" -> `['▁TO', 'MA', 'T', 'O']`. */
export function encodeBpe(pieces, text) {
  const out = [];
  for (const word of text.toUpperCase().replace(/[^A-Z' ]+/g, ' ').split(/\s+/).filter(Boolean)) {
    const chars = ('▁' + word).split(''), n = chars.length, best = Array(n + 1).fill(-Infinity), from = Array(n + 1).fill(0);
    best[0] = 0;
    for (let end = 1; end <= n; end++) for (let start = Math.max(0, end - 16); start < end; start++) {
      const score = pieces.get(chars.slice(start, end).join(''));
      if (score !== undefined && best[start] + score > best[end]) { best[end] = best[start] + score; from[end] = start; }
    }
    if (best[n] === -Infinity) continue;   // a character the model has no piece for: the word is skipped
    const parts = []; for (let e = n; e > 0; e = from[e]) parts.unshift(chars.slice(from[e], e).join(''));
    out.push(...parts);
  }
  return out;
}
