import { test } from 'node:test';
import * as assert from 'node:assert/strict';

import { languageForCell, MIME } from '../src/magic';

test('shell magics', () => {
  assert.equal(languageForCell('%%shell\ncactup show\n'), MIME.shell);
  assert.equal(languageForCell('%%shell --timeout 60\nsleep 1'), MIME.shell);
  assert.equal(languageForCell('%%bash\nls'), MIME.shell);
});

test('file magics pick the language from the extension', () => {
  assert.equal(languageForCell('%%file ~/.cactup/machines/mylab/meta.toml\n[machine]'), MIME.toml);
  assert.equal(languageForCell('%%file --append wave.par\nfoo'), MIME.par);
  assert.equal(languageForCell('%%file "my thornlist.th"'), MIME.plain);
  assert.equal(languageForCell('%%file forks.th\n!CRL_VERSION = 1.0'), MIME.thornlist);
  assert.equal(languageForCell('%%file repos/x/interface.ccl'), MIME.ccl);
  assert.equal(languageForCell('%%writefile src/wave.cxx'), MIME.cpp);
  assert.equal(languageForCell('%%file src/Sources.F90'), MIME.fortran);
  assert.equal(languageForCell('%%file submitscripts/default.py'), MIME.python);
  assert.equal(languageForCell('%%file runscripts/default.sh'), MIME.shell);
  assert.equal(languageForCell('%%file hostname.regexp'), MIME.plain);
  assert.equal(languageForCell('%%file README'), MIME.plain);
  assert.equal(languageForCell('%%file'), MIME.plain);
});

test('plain Python and unknown magics keep the notebook language', () => {
  assert.equal(languageForCell('import pandas as pd'), null);
  assert.equal(languageForCell('%show wave.par'), null);
  assert.equal(languageForCell('%%time\nx = 1'), null);
  assert.equal(languageForCell(''), null);
});
