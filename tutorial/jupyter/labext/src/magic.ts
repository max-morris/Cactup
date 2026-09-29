/**
 * Which language a notebook cell is written in, judged by its first line.
 *
 * A cell that starts with `%%shell` is a shell script, and one that starts
 * with `%%file machines/mylab/meta.toml` is TOML: the editor should
 * highlight it that way instead of as Python. `null` means "no magic we
 * know, use the notebook's language".
 */

export const MIME = {
  par: 'text/x-cactus-par',
  thornlist: 'text/x-cactus-thornlist',
  ccl: 'text/x-cactus-ccl',
  optionlist: 'text/x-cactus-optionlist',
  shell: 'text/x-sh',
  toml: 'text/x-toml',
  python: 'text/x-python',
  c: 'text/x-csrc',
  cpp: 'text/x-c++src',
  fortran: 'text/x-fortran',
  yaml: 'text/x-yaml',
  json: 'application/json',
  markdown: 'text/x-markdown',
  html: 'text/html',
  javascript: 'text/javascript',
  plain: 'text/plain'
};

const BY_EXTENSION: { [ext: string]: string } = {
  par: MIME.par,
  th: MIME.thornlist,
  ccl: MIME.ccl,
  cfg: MIME.optionlist,
  sh: MIME.shell,
  bash: MIME.shell,
  toml: MIME.toml,
  py: MIME.python,
  rpar: MIME.python,
  c: MIME.c,
  h: MIME.c,
  cc: MIME.cpp,
  cxx: MIME.cpp,
  cpp: MIME.cpp,
  hh: MIME.cpp,
  hxx: MIME.cpp,
  hpp: MIME.cpp,
  f: MIME.fortran,
  f77: MIME.fortran,
  f90: MIME.fortran,
  yaml: MIME.yaml,
  yml: MIME.yaml,
  json: MIME.json,
  md: MIME.markdown,
  txt: MIME.plain,
  regexp: MIME.plain
};

const SHELL_MAGICS = new Set(['shell', 'bash', 'sh']);
const FILE_MAGICS = new Set(['file', 'writefile']);

/** The MIME type for a file name, from its extension; `null` if unknown. */
export function mimeForPath(path: string): string | null {
  const base = path.split('/').pop() ?? '';
  if (/^make\.(code|configuration)\.defn$/.test(base)) {
    return MIME.shell;
  }
  const dot = base.lastIndexOf('.');
  if (dot < 0) {
    return null;
  }
  return BY_EXTENSION[base.slice(dot + 1).toLowerCase()] ?? null;
}

/** The MIME type a cell's first line asks for, or `null` for none. */
export function languageForCell(source: string): string | null {
  const nl = source.indexOf('\n');
  const first = (nl < 0 ? source : source.slice(0, nl)).trim();
  const m = /^%%([A-Za-z_]\w*)\s*(.*)$/.exec(first);
  if (!m) {
    return null;
  }
  const [, magic, rest] = m;
  if (SHELL_MAGICS.has(magic)) {
    return MIME.shell;
  }
  if (FILE_MAGICS.has(magic)) {
    // The path is the first argument that is not an option.
    const path = rest
      .split(/\s+/)
      .map(word => word.replace(/^['"]|['"]$/g, ''))
      .find(word => word.length > 0 && !word.startsWith('-'));
    return path ? mimeForPath(path) ?? MIME.plain : MIME.plain;
  }
  if (magic === 'html') {
    return MIME.html;
  }
  if (magic === 'javascript' || magic === 'js') {
    return MIME.javascript;
  }
  if (magic === 'markdown') {
    return MIME.markdown;
  }
  return null;
}
