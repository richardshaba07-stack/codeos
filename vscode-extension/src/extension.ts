// Punto d'ingresso dell'estensione CodeOS.
//
// Fa da client del server gRPC (`codeos-server`): indicizza progetto/file,
// interroga il grafo e — soprattutto — apre lo stream `WatchEvents` per
// mostrare LIVE le violazioni del sistema immunitario architetturale dentro
// l'editor (toast + canale di output + barra di stato).

import * as path from 'path';
import * as vscode from 'vscode';
import {
  ArchitectureReport,
  CodeOsClient,
  CodeOsEvent,
  SourceLocation,
  ViolationEvent,
} from './client';
import { ArchitectureTreeProvider } from './sidebar';

let client: CodeOsClient | undefined;
let stopWatchFn: (() => void) | undefined;
let output: vscode.OutputChannel;
let statusBar: vscode.StatusBarItem;
let diagnostics: vscode.DiagnosticCollection;
/** Vista ad albero nella Activity Bar: lo spazio negativo + le violazioni live. */
let sidebar: ArchitectureTreeProvider;
/** Timer di debounce per l'aggiornamento della sidebar dopo i `graphUpdated`. */
let sidebarRefreshTimer: ReturnType<typeof setTimeout> | undefined;
/** Timer di debounce per l'auto-indicizzazione al salvataggio. */
let indexOnSaveTimer: ReturnType<typeof setTimeout> | undefined;
/** Diagnostiche di violazione accumulate per file (fsPath → lista). */
const violationsByFile = new Map<string, vscode.Diagnostic[]>();

export function activate(context: vscode.ExtensionContext): void {
  output = vscode.window.createOutputChannel('CodeOS');
  diagnostics = vscode.languages.createDiagnosticCollection('codeos');
  statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  statusBar.command = 'codeos.toggleWatch';
  setDisconnected();
  statusBar.show();
  sidebar = new ArchitectureTreeProvider();
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider('codeos.architecture', sidebar),
  );
  context.subscriptions.push(output, statusBar, diagnostics, {
    dispose: () => deactivate(),
  });

  context.subscriptions.push(
    vscode.commands.registerCommand('codeos.indexProject', () => indexProject(context)),
    vscode.commands.registerCommand('codeos.indexFile', () => indexFile(context)),
    vscode.commands.registerCommand('codeos.query', () => runQuery(context)),
    vscode.commands.registerCommand('codeos.architectureReport', () =>
      architectureReport(context),
    ),
    vscode.commands.registerCommand('codeos.refreshSidebar', () => refreshSidebar(context)),
    vscode.commands.registerCommand('codeos.revealLocation', (loc: SourceLocation) =>
      revealLocation(loc),
    ),
    vscode.commands.registerCommand('codeos.toggleWatch', () => toggleWatch(context)),
  );

  const autoConnect = vscode.workspace
    .getConfiguration('codeos')
    .get<boolean>('autoConnect', true);
  if (autoConnect) {
    startWatch(context);
  }

  // Auto-index on save:
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument((document) => {
      const autoIndex = vscode.workspace
        .getConfiguration('codeos')
        .get<boolean>('autoIndexOnSave', true);
      if (!autoIndex) {
        return;
      }
      
      const file = document.uri.fsPath;
      const ext = path.extname(file).toLowerCase();
      const supported = ['.rs', '.ts', '.tsx', '.js', '.jsx', '.go', '.java', '.py'];
      if (!supported.includes(ext)) {
        return;
      }

      // Clear stale diagnostic errors for the saved file
      diagnostics.delete(document.uri);
      violationsByFile.delete(file);

      // Debounce (500ms) to avoid overlapping runs
      if (indexOnSaveTimer) {
        clearTimeout(indexOnSaveTimer);
      }
      indexOnSaveTimer = setTimeout(() => {
        indexOnSaveTimer = undefined;
        log(`Auto-indicizzo dopo il salvataggio: ${path.basename(file)}`);
        getClient(context).indexFiles([file]).catch((err) => {
          log(`Auto-indicizzazione fallita per ${file}: ${err}`);
        });
      }, 500);
    })
  );
}

export function deactivate(): void {
  stopWatch();
  if (sidebarRefreshTimer) {
    clearTimeout(sidebarRefreshTimer);
    sidebarRefreshTimer = undefined;
  }
  client?.close();
  client = undefined;
}

// ---------------------------------------------------------------------------
// Connessione
// ---------------------------------------------------------------------------

function getClient(context: vscode.ExtensionContext): CodeOsClient {
  if (!client) {
    const address = vscode.workspace
      .getConfiguration('codeos')
      .get<string>('serverAddress', '127.0.0.1:50051');
    const protoPath = path.join(context.extensionPath, 'proto', 'codeos.proto');
    client = new CodeOsClient(address, protoPath);
    log(`client creato verso ${address}`);
  }
  return client;
}

// ---------------------------------------------------------------------------
// Comandi
// ---------------------------------------------------------------------------

async function indexProject(context: vscode.ExtensionContext): Promise<void> {
  const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (!root) {
    vscode.window.showWarningMessage('CodeOS: nessuna cartella di lavoro aperta.');
    return;
  }
  await withProgress(`Indicizzo il progetto ${path.basename(root)}...`, async () => {
    await getClient(context).indexProject(root);
  });
  log(`IndexProject completato: ${root}`);
  vscode.window.showInformationMessage('CodeOS: progetto indicizzato.');
  // Il grafo è appena cambiato: aggiorna lo spazio negativo nella sidebar.
  void refreshSidebar(context);
}

async function indexFile(context: vscode.ExtensionContext): Promise<void> {
  const file = vscode.window.activeTextEditor?.document.uri.fsPath;
  if (!file) {
    vscode.window.showWarningMessage('CodeOS: nessun file attivo da indicizzare.');
    return;
  }
  await withProgress(`Indicizzo ${path.basename(file)}...`, async () => {
    await getClient(context).indexFiles([file]);
  });
  log(`IndexFiles completato: ${file}`);
  vscode.window.showInformationMessage(`CodeOS: indicizzato ${path.basename(file)}.`);
}

async function runQuery(context: vscode.ExtensionContext): Promise<void> {
  const text = await vscode.window.showInputBox({
    title: 'CodeOS — Interroga il grafo',
    prompt: 'Cosa vuoi fare? (es. "voglio aggiungere il login OAuth")',
    placeHolder: 'Descrivi il cambiamento o la domanda in linguaggio naturale',
  });
  if (!text) {
    return;
  }

  let result;
  try {
    result = await withProgress('Costruisco il contesto...', () =>
      getClient(context).queryGraph(text),
    );
  } catch (err) {
    reportError('Query fallita', err);
    return;
  }

  const doc = await vscode.workspace.openTextDocument({
    content: result.formattedContext || '(nessun contesto rilevante trovato)',
    language: 'markdown',
  });
  await vscode.window.showTextDocument(doc, { preview: false });
  log(`Query "${text}": ${result.entities.length} entità, ${result.relations.length} relazioni`);
}

async function architectureReport(context: vscode.ExtensionContext): Promise<void> {
  let report: ArchitectureReport;
  try {
    report = await withProgress('Leggo lo spazio negativo dell\'architettura...', () =>
      getClient(context).getArchitectureReport(),
    );
  } catch (err) {
    reportError('Referto architetturale fallito', err);
    return;
  }

  sidebar.setReport(report);
  const doc = await vscode.workspace.openTextDocument({
    content: renderReport(report),
    language: 'markdown',
  });
  await vscode.window.showTextDocument(doc, { preview: false });
  log(
    `Referto: ${report.invariants.length} invarianti, ${report.fossils.length} fossili, ` +
      `${report.gaps.length} lacune`,
  );
}

/** Rende il referto architetturale in Markdown leggibile per l'editor. */
function renderReport(report: ArchitectureReport): string {
  const lines: string[] = [];
  lines.push('# Referto architetturale di CodeOS');
  lines.push('');
  lines.push(
    'Gli invarianti impliciti scoperti leggendo lo *spazio negativo* della codebase, ' +
      'lungo i quattro assi: struttura, tempo, intento e meta.',
  );
  lines.push('');

  // Asse struttura (+ tempo): gli invarianti di layering.
  lines.push('## Invarianti di layering');
  lines.push('');
  if (report.invariants.length === 0) {
    lines.push('_Nessun invariante di layering scoperto (grafo troppo piccolo o assente)._');
  } else {
    lines.push('| Fondazione (upstream) | Dipende (downstream) | Support | Confidenza | Calibrata |');
    lines.push('| --- | --- | ---: | ---: | :---: |');
    for (const inv of report.invariants) {
      const conf = (inv.confidence * 100).toFixed(1) + '%';
      const cal = inv.calibrated ? 'sì (tempo)' : 'no (strutturale)';
      lines.push(
        `| \`${inv.upstream}\` | \`${inv.downstream}\` | ${inv.support} | ${conf} | ${cal} |`,
      );
    }
  }
  lines.push('');

  // Asse intento: i Fossili di Decisione.
  lines.push('## Fossili di Decisione');
  lines.push('');
  lines.push('_Quando e perché ciascun confine è nato (dalla storia git)._');
  lines.push('');
  if (report.fossils.length === 0) {
    lines.push('_Nessun fossile: storia git non agganciata o confini mai co-toccati._');
  } else {
    for (const f of report.fossils) {
      const when = f.bornAtUnix > 0 ? new Date(f.bornAtUnix * 1000).toISOString() : '(data ignota)';
      const shortHash = f.bornAt ? f.bornAt.slice(0, 12) : '(sconosciuto)';
      lines.push(`### \`${f.downstream}\` → \`${f.upstream}\``);
      lines.push('');
      lines.push(`- **Nato nel commit:** \`${shortHash}\` (${when})`);
      lines.push(`- **Intento:** ${f.intent || '_(nessun messaggio di commit)_'}`);
      if (f.bornStructure.length > 0) {
        lines.push(`- **Diff di cristallizzazione:** ${f.bornStructure.map((s) => `\`${s}\``).join(', ')}`);
      }
      lines.push('');
    }
  }

  // Asse meta: lo spazio negativo del secondo ordine.
  lines.push('## Lacune del secondo ordine');
  lines.push('');
  lines.push(
    '_Gli invarianti che **mancano** dove la convenzione architetturale direbbe che ' +
      'dovrebbero esserci: quasi sempre debito tecnico o un bug latente._',
  );
  lines.push('');
  if (report.gaps.length === 0) {
    lines.push('_Nessuna lacuna: ogni fondazione è rispettata senza eccezioni._');
  } else {
    lines.push('| Fondazione | Eccezione (accoppiata in entrambi i versi) | Layer che la rispettano |');
    lines.push('| --- | --- | ---: |');
    for (const g of report.gaps) {
      lines.push(`| \`${g.upstream}\` | \`${g.downstream}\` | ${g.foundationSupport} |`);
    }
  }
  lines.push('');

  return lines.join('\n');
}

function toggleWatch(context: vscode.ExtensionContext): void {
  if (stopWatchFn) {
    stopWatch();
    vscode.window.showInformationMessage('CodeOS: watch fermato.');
  } else {
    startWatch(context);
  }
}

// ---------------------------------------------------------------------------
// Sidebar (vista ad albero dello spazio negativo)
// ---------------------------------------------------------------------------

/** Comando del pulsante ↻: rilegge il referto e lo riversa nella sidebar. */
async function refreshSidebar(context: vscode.ExtensionContext): Promise<void> {
  let report: ArchitectureReport;
  try {
    report = await withProgress('Aggiorno lo spazio negativo...', () =>
      getClient(context).getArchitectureReport(),
    );
  } catch (err) {
    reportError('Aggiornamento sidebar fallito', err);
    return;
  }
  sidebar.setReport(report);
  log(
    `Sidebar aggiornata: ${report.invariants.length} invarianti, ` +
      `${report.fossils.length} fossili, ${report.gaps.length} lacune`,
  );
}

/** Coalizza una raffica di `graphUpdated` in un solo refresh, poco dopo l'ultimo. */
function scheduleSidebarRefresh(): void {
  if (sidebarRefreshTimer) {
    clearTimeout(sidebarRefreshTimer);
  }
  sidebarRefreshTimer = setTimeout(() => {
    sidebarRefreshTimer = undefined;
    void refreshSidebarSilently();
  }, 1500);
}

/** Refresh senza progress né toast (riusa il client già aperto dal watch). */
async function refreshSidebarSilently(): Promise<void> {
  if (!client) {
    return;
  }
  try {
    const report = await client.getArchitectureReport();
    sidebar.setReport(report);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    log(`Aggiornamento sidebar (silenzioso) fallito: ${message}`);
  }
}

/** Comando `codeos.revealLocation`: apre il file e salta alla posizione cliccata. */
async function revealLocation(loc: SourceLocation): Promise<void> {
  if (!loc?.filePath) {
    return;
  }
  const range = locationRange(loc);
  const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(loc.filePath));
  const editor = await vscode.window.showTextDocument(doc);
  editor.selection = new vscode.Selection(range.start, range.end);
  editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
}

// ---------------------------------------------------------------------------
// Stream eventi (il sistema immunitario, live)
// ---------------------------------------------------------------------------

function startWatch(context: vscode.ExtensionContext): void {
  if (stopWatchFn) {
    return; // già attivo
  }
  let active: CodeOsClient;
  try {
    active = getClient(context);
  } catch (err) {
    reportError('Connessione fallita', err);
    return;
  }

  // Sessione nuova: lo stream consegna solo eventi futuri, quindi le vecchie
  // diagnostiche non verrebbero ripubblicate. Le azzeriamo per non lasciare stale.
  clearDiagnostics();
  sidebar.clearViolations();
  setConnected();
  log('WatchEvents: stream aperto.');
  stopWatchFn = active.watchEvents({
    onEvent: handleEvent,
    onError: (err) => {
      setDisconnected();
      stopWatchFn = undefined;
      log(`WatchEvents errore: ${err.message}`);
    },
    onEnd: () => {
      setDisconnected();
      stopWatchFn = undefined;
      log('WatchEvents: stream chiuso dal server.');
    },
  });
}

function stopWatch(): void {
  if (stopWatchFn) {
    stopWatchFn();
    stopWatchFn = undefined;
  }
  setDisconnected();
}

function handleEvent(event: CodeOsEvent): void {
  switch (event.type) {
    case 'filesIndexed':
      log(`> FilesIndexed: ${event.filePaths.length} file`);
      break;
    case 'graphUpdated':
      log(
        `> GraphUpdated: +${event.addedEntities} entità / +${event.addedRelations} relazioni ` +
          `(-${event.removedEntities} / -${event.removedRelations})`,
      );
      statusBar.text = '$(pulse) CodeOS';
      // Il grafo è cambiato: lo spazio negativo potrebbe essere diverso. Aggiorna
      // la sidebar, ma con debounce: durante un'indicizzazione gli eventi arrivano
      // a raffica e ricalcolare il referto a ogni colpo sarebbe sprecone.
      scheduleSidebarRefresh();
      break;
    case 'violation': {
      const v = event.violation;
      const msg = `⚠️ Violazione architetturale: ${v.message}`;
      log(`> ${msg} [rule=${v.ruleId} src=${v.sourceId} -> dst=${v.targetId}]`);
      statusBar.text = '$(alert) CodeOS';
      statusBar.tooltip = msg;
      sidebar.addViolation(v);
      addViolationDiagnostic(v);
      vscode.window
        .showWarningMessage(msg, 'Mostra problema', 'Mostra log')
        .then((choice) => {
          if (choice === 'Mostra log') {
            output.show(true);
          } else if (choice === 'Mostra problema') {
            revealViolation(v);
          }
        });
      break;
    }
    case 'indexProgress': {
      const total = event.totalFiles;
      const processed = event.processedFiles;
      const skipped = event.skippedFiles;
      const errors = event.parseErrors;
      const current = event.currentFile;
      
      statusBar.text = `$(sync~spin) CodeOS: ${processed}/${total} file`;
      statusBar.tooltip = `Indicizzazione in corso: ${current}\nSaltati: ${skipped}, Errori: ${errors}`;
      break;
    }
  }
}

// ---------------------------------------------------------------------------
// Diagnostiche: il sistema immunitario nel pannello "Problemi"
// ---------------------------------------------------------------------------

/** Converte una posizione (riga 1-based, colonna 0-based) in un `Range` VS Code (0-based). */
function locationRange(loc?: SourceLocation): vscode.Range {
  const startLine = Math.max(0, (loc?.startLine ?? 1) - 1);
  const startCol = Math.max(0, loc?.startColumn ?? 0);
  const endLine = Math.max(startLine, (loc?.endLine ?? loc?.startLine ?? 1) - 1);
  const endCol = Math.max(0, loc?.endColumn ?? 0);
  return new vscode.Range(startLine, startCol, endLine, endCol);
}

/** Range della posizione associata a una violazione. */
function violationRange(v: ViolationEvent): vscode.Range {
  return locationRange(v.location);
}

/** Mappa la severità di CodeOS sul livello di diagnostica di VS Code. */
function diagnosticSeverity(severity?: string): vscode.DiagnosticSeverity {
  switch (severity) {
    case 'high_risk':
      return vscode.DiagnosticSeverity.Error;
    case 'info':
      return vscode.DiagnosticSeverity.Information;
    case 'warning':
    default:
      return vscode.DiagnosticSeverity.Warning;
  }
}

function addViolationDiagnostic(v: ViolationEvent): void {
  const filePath = v.location?.filePath;
  if (!filePath) {
    return; // senza posizione non possiamo ancorare una diagnostica
  }
  const diagnostic = new vscode.Diagnostic(
    violationRange(v),
    v.message,
    diagnosticSeverity(v.severity),
  );
  diagnostic.source = 'CodeOS';
  diagnostic.code = 'layering-violation';

  const existing = violationsByFile.get(filePath) ?? [];
  // Evita duplicati esatti (stessa riga + stesso messaggio) su eventi ripetuti.
  const isDuplicate = existing.some(
    (d) => d.range.isEqual(diagnostic.range) && d.message === diagnostic.message,
  );
  if (!isDuplicate) {
    existing.push(diagnostic);
    violationsByFile.set(filePath, existing);
    diagnostics.set(vscode.Uri.file(filePath), existing);
  }
}

async function revealViolation(v: ViolationEvent): Promise<void> {
  const filePath = v.location?.filePath;
  if (!filePath) {
    output.show(true);
    return;
  }
  const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(filePath));
  const editor = await vscode.window.showTextDocument(doc);
  const range = violationRange(v);
  editor.selection = new vscode.Selection(range.start, range.end);
  editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
}

function clearDiagnostics(): void {
  violationsByFile.clear();
  diagnostics.clear();
}

// ---------------------------------------------------------------------------
// UI helpers
// ---------------------------------------------------------------------------

function setConnected(): void {
  statusBar.text = '$(pulse) CodeOS';
  statusBar.tooltip = 'CodeOS: connesso (watch attivo) — clicca per fermare';
}

function setDisconnected(): void {
  statusBar.text = '$(circle-slash) CodeOS';
  statusBar.tooltip = 'CodeOS: non connesso — clicca per avviare il watch';
}

function withProgress<T>(title: string, task: () => Thenable<T>): Thenable<T> {
  return vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title, cancellable: false },
    () => task(),
  );
}

function reportError(prefix: string, err: unknown): void {
  const message = err instanceof Error ? err.message : String(err);
  log(`${prefix}: ${message}`);
  vscode.window.showErrorMessage(`CodeOS: ${prefix} — ${message}`);
}

function log(line: string): void {
  const stamp = new Date().toISOString();
  output.appendLine(`[${stamp}] ${line}`);
}
