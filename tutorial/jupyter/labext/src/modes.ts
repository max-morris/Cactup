/**
 * CodeMirror stream modes for the Cactus file formats: parameter files,
 * thornlists, CCL and optionlists.
 *
 * They use the legacy token names (`keyword`, `def`, `variable-2`, ...), which
 * StreamLanguage maps onto highlight tags, so the active JupyterLab theme
 * colors them like any built-in language.
 */

import { StreamParser, StringStream } from '@codemirror/language';

// cactup's template tokens: @VAR@, @ENV(NAME)@, @KNOB(name)@ and their
// -OPTIONAL forms, and the @@ escape.
export const TEMPLATE_TOKEN = /^@(?:@|[A-Z][A-Z0-9_]*@|(?:ENV|KNOB)(?:-OPTIONAL)?\([^)\n]*\)@)/;
const NUMBER = /^[+-]?(?:\d+\.\d*|\.\d+|\d+)(?:[eEdD][+-]?\d+)?/;

interface ParState {
  inString: boolean;
  afterScope: boolean;
}

export const parMode: StreamParser<ParState> = {
  name: 'cactus-par',
  startState: () => ({ inString: false, afterScope: false }),
  token(stream: StringStream, state: ParState): string | null {
    if (state.inString) {
      if (stream.match(TEMPLATE_TOKEN)) {
        return 'variable-3';
      }
      while (!stream.eol()) {
        const ch = stream.peek();
        if (ch === '"') {
          stream.next();
          state.inString = false;
          return 'string';
        }
        if (ch === '@' && stream.match(TEMPLATE_TOKEN, false)) {
          return 'string';
        }
        stream.next();
      }
      return 'string';
    }
    if (stream.eatSpace()) {
      return null;
    }
    if (stream.match(/^#.*/)) {
      return 'comment';
    }
    if (stream.match(TEMPLATE_TOKEN)) {
      return 'variable-3';
    }
    if (stream.match(/^ActiveThorns\b/i)) {
      return 'keyword';
    }
    if (state.afterScope && stream.match(/^[A-Za-z_]\w*/)) {
      state.afterScope = false;
      return 'property';
    }
    if (stream.match(/^[A-Za-z_]\w*(?=::)/)) {
      return 'def';
    }
    if (stream.match('::')) {
      state.afterScope = true;
      return 'operator';
    }
    if (stream.match(/^\$[A-Za-z_]\w*/)) {
      return 'variable-2';
    }
    if (stream.peek() === '"') {
      stream.next();
      state.inString = true;
      return 'string';
    }
    if (stream.match(/^(?:yes|no|true|false)\b/i)) {
      return 'atom';
    }
    if (stream.match(NUMBER)) {
      return 'number';
    }
    if (stream.match(/^[=*/+\-]/)) {
      return 'operator';
    }
    if (stream.match(/^[[\](),]/)) {
      return 'bracket';
    }
    stream.next();
    return null;
  },
  languageData: { commentTokens: { line: '#' } }
};

interface ThornlistState {
  inValue: boolean;
}

export const thornlistMode: StreamParser<ThornlistState> = {
  name: 'cactus-thornlist',
  startState: () => ({ inValue: false }),
  token(stream: StringStream, state: ThornlistState): string | null {
    if (stream.sol()) {
      state.inValue = false;
    }
    if (state.inValue) {
      if (stream.match(/^\$\w+/)) {
        return 'variable-2';
      }
      stream.eatWhile(/[^$]/);
      if (stream.eol()) {
        state.inValue = false;
      }
      return 'string';
    }
    if (stream.eatSpace()) {
      return null;
    }
    if (stream.match(/^#DISABLED\b/)) {
      return 'meta';
    }
    if (stream.match(/^#.*/)) {
      return 'comment';
    }
    if (stream.match(/^![A-Za-z_]\w*/)) {
      return 'keyword';
    }
    if (stream.match('=')) {
      state.inValue = true;
      return 'operator';
    }
    if (stream.match(/^[\w-]+\/[\w-]+/)) {
      return 'def';
    }
    if (stream.match(/^\$\w+/)) {
      return 'variable-2';
    }
    stream.next();
    return null;
  },
  languageData: { commentTokens: { line: '#' } }
};

const CCL_KEYWORDS = new Set(
  (
    'implements inherits friend includes include source in uses provides requires ' +
    'function subroutine with private public protected restricted shares extends ' +
    'schedule group before after while if as lang storage trigger triggers sync ' +
    'reads writes options option tags type dim timelevels size distrib ' +
    'global local level singlemap array scalar gf ' +
    'steerable accumulator accumulator_base never always recover ' +
    'optional optional_ifactive thorn thorns centering'
  ).split(' ')
);

const CCL_TYPES = new Set(
  (
    'int real keyword string boolean cctk_int cctk_real cctk_real4 cctk_real8 ' +
    'cctk_real2 cctk_complex cctk_pointer cctk_pointer_to_const cctk_string void'
  ).split(' ')
);

const CCL_BINS = new Set(
  (
    'cctk_startup cctk_wragh cctk_paramcheck cctk_basegrid cctk_initial ' +
    'cctk_postinitial cctk_poststep cctk_prestep cctk_evol cctk_analysis ' +
    'cctk_checkpoint cctk_terminate cctk_shutdown cctk_recover_variables ' +
    'cctk_recover_parameters cctk_postrestrict cctk_postregrid ' +
    'cctk_postregridinitial cctk_cpinitial cctk_postrestrictinitial'
  ).split(' ')
);

// CCL strings come in both quotes: "a description", TAGS='index={0 0 0}'.
export const cclMode: StreamParser<{ quote: string | null }> = {
  name: 'cactus-ccl',
  startState: () => ({ quote: null }),
  token(stream: StringStream, state: { quote: string | null }): string | null {
    if (state.quote) {
      while (!stream.eol()) {
        const ch = stream.next();
        if (ch === '\\') {
          stream.next();
        } else if (ch === state.quote) {
          state.quote = null;
          break;
        }
      }
      return 'string';
    }
    if (stream.eatSpace()) {
      return null;
    }
    if (stream.match(/^#.*/)) {
      return 'comment';
    }
    const quote = stream.peek();
    if (quote === '"' || quote === "'") {
      stream.next();
      state.quote = quote;
      return 'string';
    }
    if (stream.match(NUMBER)) {
      return 'number';
    }
    const word = stream.match(/^[A-Za-z_]\w*/) as RegExpMatchArray | null;
    if (word) {
      const w = word[0].toLowerCase();
      if (CCL_BINS.has(w)) {
        return 'atom';
      }
      if (CCL_TYPES.has(w)) {
        // Styled as a keyword, as %show does: the themes give `type` no color.
        return 'keyword';
      }
      if (CCL_KEYWORDS.has(w)) {
        return 'keyword';
      }
      return stream.match(/^\s*::/, false) ? 'def' : 'variable';
    }
    if (stream.match(/^(?:::|\.\.|[*:=,])/)) {
      return 'operator';
    }
    if (stream.match(/^[{}()[\]]/)) {
      return 'bracket';
    }
    stream.next();
    return null;
  },
  languageData: { commentTokens: { line: '#' } }
};

export const optionlistMode: StreamParser<{ inValue: boolean }> = {
  name: 'cactus-optionlist',
  startState: () => ({ inValue: false }),
  token(stream: StringStream, state: { inValue: boolean }): string | null {
    if (stream.sol()) {
      state.inValue = false;
      if (stream.match(/^\s*#.*/)) {
        return 'comment';
      }
      if (stream.match(/^\s*[A-Za-z_]\w*(?=\s*=)/)) {
        return 'def';
      }
    }
    if (stream.eatSpace()) {
      return null;
    }
    if (!state.inValue && stream.match('=')) {
      state.inValue = true;
      return 'operator';
    }
    if (stream.match(TEMPLATE_TOKEN)) {
      return 'variable-3';
    }
    if (stream.match(/^\$\{?\w+\}?/)) {
      return 'variable-2';
    }
    stream.eatWhile(/[^@$\s]/);
    if (stream.current().length === 0) {
      stream.next();
    }
    return state.inValue ? 'string' : null;
  },
  languageData: { commentTokens: { line: '#' } }
};
