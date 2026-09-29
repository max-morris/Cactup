/**
 * JupyterLab plugin for the Cactup tutorial.
 *
 * - Registers the Cactus file formats (parameter files, thornlists, CCL,
 *   optionlists) as editor languages, for notebook cells and for files opened
 *   in the file editor.
 * - Highlights each code cell in the language its first-line magic names:
 *   `%%shell` cells as shell, `%%file x.toml` cells as TOML, and so on.
 * - Marks the magic line itself, and cactup's `@VAR@` template tokens, in
 *   every editor.
 */

import { JupyterFrontEnd, JupyterFrontEndPlugin } from '@jupyterlab/application';
import { ICellModel, ICodeCellModel } from '@jupyterlab/cells';
import {
  EditorExtensionRegistry,
  IEditorExtensionRegistry,
  IEditorLanguageRegistry
} from '@jupyterlab/codemirror';
import { INotebookTracker, Notebook, NotebookPanel } from '@jupyterlab/notebook';
import { LanguageSupport, StreamLanguage, StreamParser } from '@codemirror/language';
import { RangeSetBuilder } from '@codemirror/state';
import {
  Decoration,
  DecorationSet,
  EditorView,
  MatchDecorator,
  ViewPlugin,
  ViewUpdate
} from '@codemirror/view';

import { languageForCell, MIME } from './magic';
import { cclMode, optionlistMode, parMode, thornlistMode } from './modes';

function support(mode: StreamParser<unknown>): () => Promise<LanguageSupport> {
  return async () => new LanguageSupport(StreamLanguage.define(mode));
}

function registerLanguages(languages: IEditorLanguageRegistry): void {
  languages.addLanguage({
    name: 'Cactus parameter file',
    displayName: 'Cactus parameter file',
    alias: ['cactus-par', 'par'],
    mime: MIME.par,
    extensions: ['par'],
    load: support(parMode as StreamParser<unknown>)
  });
  languages.addLanguage({
    name: 'Cactus thornlist',
    displayName: 'Cactus thornlist',
    alias: ['thornlist', 'crl'],
    mime: MIME.thornlist,
    extensions: ['th'],
    load: support(thornlistMode as StreamParser<unknown>)
  });
  languages.addLanguage({
    name: 'Cactus CCL',
    displayName: 'Cactus CCL',
    alias: ['ccl'],
    mime: MIME.ccl,
    extensions: ['ccl'],
    load: support(cclMode as StreamParser<unknown>)
  });
  languages.addLanguage({
    name: 'Cactus optionlist',
    displayName: 'Cactus optionlist',
    alias: ['optionlist'],
    mime: MIME.optionlist,
    extensions: ['cfg'],
    load: support(optionlistMode as StreamParser<unknown>)
  });
  // JupyterLab's built-in TTCN configuration mode also claims `.cfg`, and the
  // first registration wins; in a Cactus tree a `.cfg` is an optionlist.
  const ttcn = languages.findByName('TTCN_CFG') as { extensions?: string[] } | null;
  if (ttcn?.extensions) {
    ttcn.extensions = ttcn.extensions.filter(ext => ext !== 'cfg');
  }
  // A thorn's make.code.defn and make.configuration.defn are make fragments;
  // shell highlighting reads them well.
  languages.addLanguage({
    name: 'Cactus make definitions',
    displayName: 'Cactus make definitions',
    mime: 'text/x-cactus-make-defn',
    filename: /^make\.(code|configuration)\.defn$/,
    load: async () => {
      const shell = languages.findByMIME(MIME.shell);
      if (!shell) {
        throw new Error('no shell language registered');
      }
      return languages.getLanguage(shell.name).then(spec => spec!.support!);
    }
  });
}

// -- editor decorations ------------------------------------------------------

const templateMark = Decoration.mark({ class: 'cm-cactup-template' });
const templateMatcher = new MatchDecorator({
  regexp: /@(?:@|[A-Z][A-Z0-9_]*@|(?:ENV|KNOB)(?:-OPTIONAL)?\([^)\n]*\)@)/g,
  decoration: () => templateMark
});

const templateTokens = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = templateMatcher.createDeco(view);
    }
    update(update: ViewUpdate) {
      this.decorations = templateMatcher.updateDeco(update, this.decorations);
    }
  },
  { decorations: v => v.decorations }
);

const magicLineDeco = Decoration.line({ class: 'cm-cactup-magic-line' });

function magicLineDecorations(view: EditorView): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  const first = view.state.doc.line(1);
  if (/^%%?[A-Za-z_]/.test(first.text)) {
    builder.add(first.from, first.from, magicLineDeco);
  }
  return builder.finish();
}

const magicLine = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = magicLineDecorations(view);
    }
    update(update: ViewUpdate) {
      if (update.docChanged) {
        this.decorations = magicLineDecorations(update.view);
      }
    }
  },
  { decorations: v => v.decorations }
);

// -- per-cell languages ------------------------------------------------------

function watchNotebook(notebook: Notebook): void {
  const watched = new WeakSet<ICellModel>();

  // Setting a cell's MIME type is how the editor learns its language. The
  // notebook resets every cell to the kernel's language when the kernel
  // starts or changes, so the choice is re-applied whenever that happens.
  const apply = (model: ICodeCellModel): void => {
    const want = languageForCell(model.sharedModel.getSource()) ?? notebook.codeMimetype;
    if (model.mimeType !== want) {
      model.mimeType = want;
    }
  };

  const watch = (model: ICellModel): void => {
    if (model.type !== 'code' || watched.has(model)) {
      return;
    }
    watched.add(model);
    const code = model as ICodeCellModel;
    apply(code);
    code.sharedModel.changed.connect(() => apply(code));
    code.mimeTypeChanged.connect(() => apply(code));
  };

  const watchAll = (): void => {
    const cells = notebook.model?.cells;
    if (!cells) {
      return;
    }
    for (let i = 0; i < cells.length; i++) {
      watch(cells.get(i));
    }
  };

  const connect = (): void => {
    watchAll();
    notebook.model?.cells.changed.connect(watchAll);
  };
  notebook.modelChanged.connect(connect);
  connect();
}

const plugin: JupyterFrontEndPlugin<void> = {
  id: 'cactup-tutorial:plugin',
  description: 'Cactus languages and magic-aware cell highlighting for the Cactup tutorial.',
  autoStart: true,
  requires: [IEditorLanguageRegistry, IEditorExtensionRegistry, INotebookTracker],
  activate: (
    _app: JupyterFrontEnd,
    languages: IEditorLanguageRegistry,
    extensions: IEditorExtensionRegistry,
    tracker: INotebookTracker
  ) => {
    registerLanguages(languages);
    extensions.addExtension({
      name: 'cactup-tutorial:decorations',
      factory: () => EditorExtensionRegistry.createImmutableExtension([templateTokens, magicLine])
    });
    tracker.widgetAdded.connect((_: INotebookTracker, panel: NotebookPanel) => {
      watchNotebook(panel.content);
    });
  }
};

export default plugin;
