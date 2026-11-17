// The sherpa-onnx wasm build is made for Node and calls require("path") unconditionally; the browser needs only these string functions.
(() => {
  const normalizeArray = (parts, allowAbove) => { const out = []; for (const p of parts) { if (!p || p === '.') continue; if (p === '..') { if (out.length && out.at(-1) !== '..') out.pop(); else if (allowAbove) out.push('..'); } else out.push(p); } return out; };
  const isAbsolute = p => p.charAt(0) === '/';
  const normalize = p => { const abs = isAbsolute(p), trail = p.slice(-1) === '/'; let r = normalizeArray(p.split('/'), !abs).join('/'); if (!r && !abs) r = '.'; if (r && trail) r += '/'; return (abs ? '/' : '') + r; };
  const join = (...a) => normalize(a.filter(Boolean).join('/'));
  const resolve = (...a) => { let r = '', abs = false; for (let i = a.length - 1; i >= -1 && !abs; i--) { const p = i >= 0 ? a[i] : '/'; if (!p) continue; r = p + '/' + r; abs = isAbsolute(p); } return (abs ? '/' : '') + normalizeArray(r.split('/'), !abs).join('/') || '.'; };
  const dirname = p => { const i = p.replace(/\/+$/, '').lastIndexOf('/'); return i < 0 ? '.' : i === 0 ? '/' : p.slice(0, i); };
  const basename = (p, ext) => { let b = p.replace(/\/+$/, '').split('/').pop(); if (ext && b.endsWith(ext)) b = b.slice(0, -ext.length); return b; };
  const relative = (from, to) => { const f = resolve(from).split('/').filter(Boolean), t = resolve(to).split('/').filter(Boolean); let i = 0; while (i < f.length && i < t.length && f[i] === t[i]) i++; return [...f.slice(i).map(() => '..'), ...t.slice(i)].join('/'); };
  const path = {isAbsolute, normalize, join, resolve, dirname, basename, relative, extname: p => { const b = basename(p), i = b.lastIndexOf('.'); return i > 0 ? b.slice(i) : ''; }, sep: '/'};
  path.posix = path;
  window.require = name => { if (name === 'path') return path; throw Error('require(' + name + ') is not available in the browser'); };
})();
