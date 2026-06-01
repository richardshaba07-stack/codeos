// Sidebar di CodeOS: una vista ad albero nativa nella Activity Bar che mostra
// LIVE lo "spazio negativo" dell'architettura (invarianti di layering, Fossili
// di Decisione, lacune del 2° ordine) e accumula le violazioni che arrivano
// dallo stream `WatchEvents`.
//
// È un `TreeDataProvider` nativo (niente webview/React): riusa il client gRPC
// esistente, sta dentro il budget di dipendenze del progetto e si integra con
// il tema dell'editor tramite `ThemeIcon`/`ThemeColor`. La struttura è
// volutamente piatta — sezioni in cima, foglie sotto — perché il referto è
// piccolo e si vuole leggerlo a colpo d'occhio.

import * as vscode from 'vscode';
import {
  ArchitecturalGap,
  ArchitectureReport,
  DecisionFossil,
  LayeringInvariant,
  SourceLocation,
  ViolationEvent,
} from './client';

const EMPTY_REPORT: ArchitectureReport = { invariants: [], fossils: [], gaps: [] };

/** Nodo dell'albero. Le sezioni portano i figli già materializzati; le foglie
 *  possono portare una posizione su cui saltare al click. */
export class CodeOsNode extends vscode.TreeItem {
  constructor(
    label: string,
    collapsibleState: vscode.TreeItemCollapsibleState,
    public readonly children: CodeOsNode[] = [],
    location?: SourceLocation,
  ) {
    super(label, collapsibleState);
    if (location && location.filePath) {
      this.command = {
        command: 'codeos.revealLocation',
        title: 'Apri la posizione',
        arguments: [location],
      };
    }
  }
}

export class ArchitectureTreeProvider implements vscode.TreeDataProvider<CodeOsNode> {
  private readonly emitter = new vscode.EventEmitter<CodeOsNode | undefined | void>();
  readonly onDidChangeTreeData = this.emitter.event;

  private report: ArchitectureReport = EMPTY_REPORT;
  private readonly violations: ViolationEvent[] = [];
  private statusLabel = 'Non ancora caricato — usa il pulsante ↻ o «CodeOS: Indicizza il progetto».';

  /** Sostituisce il referto mostrato e ridisegna l'albero. */
  setReport(report: ArchitectureReport): void {
    this.report = report;
    const now = new Date().toLocaleTimeString();
    this.statusLabel = `Referto aggiornato alle ${now}`;
    this.emitter.fire();
  }

  /** Aggiunge una violazione live (dedup per file+riga+messaggio). */
  addViolation(violation: ViolationEvent): void {
    const key = violationKey(violation);
    if (this.violations.some((v) => violationKey(v) === key)) {
      return;
    }
    this.violations.push(violation);
    this.emitter.fire();
  }

  /** Azzera le violazioni accumulate (nuova sessione di watch). */
  clearViolations(): void {
    if (this.violations.length === 0) {
      return;
    }
    this.violations.length = 0;
    this.emitter.fire();
  }

  getTreeItem(element: CodeOsNode): vscode.TreeItem {
    return element;
  }

  getChildren(element?: CodeOsNode): CodeOsNode[] {
    return element ? element.children : this.rootSections();
  }

  // -- costruzione dell'albero ------------------------------------------------

  private rootSections(): CodeOsNode[] {
    const status = new CodeOsNode(this.statusLabel, vscode.TreeItemCollapsibleState.None);
    status.iconPath = new vscode.ThemeIcon('info');
    status.contextValue = 'codeos.status';

    return [
      status,
      this.invariantsSection(),
      this.fossilsSection(),
      this.gapsSection(),
      this.violationsSection(),
    ];
  }

  private invariantsSection(): CodeOsNode {
    const items = this.report.invariants.map((inv) => invariantNode(inv));
    const section = sectionNode(
      'Invarianti di layering',
      items,
      'Asse struttura+tempo: quali dipendenze il codice non cabla mai.',
      'law',
    );
    return section;
  }

  private fossilsSection(): CodeOsNode {
    const items = this.report.fossils.map((f) => fossilNode(f));
    return sectionNode(
      'Fossili di Decisione',
      items,
      'Asse intento: quando e perché ogni confine è nato (storia git).',
      'history',
    );
  }

  private gapsSection(): CodeOsNode {
    const items = this.report.gaps.map((g) => gapNode(g));
    return sectionNode(
      'Lacune del 2° ordine',
      items,
      'Asse meta: l’invariante che manca dove dovrebbe esserci.',
      'search-fuzzy',
    );
  }

  private violationsSection(): CodeOsNode {
    const items = this.violations.map((v) => violationNode(v));
    return sectionNode(
      'Violazioni live',
      items,
      'Le dipendenze proibite intercettate dallo stream WatchEvents.',
      'shield',
    );
  }
}

// --- helper di costruzione dei nodi ------------------------------------------

/** Una sezione con conteggio nel description e un placeholder se vuota. */
function sectionNode(
  title: string,
  items: CodeOsNode[],
  tooltip: string,
  icon: string,
): CodeOsNode {
  const children = items.length > 0 ? items : [emptyNode()];
  // Espansa di default se ha contenuto, così il referto è visibile subito.
  const state =
    items.length > 0
      ? vscode.TreeItemCollapsibleState.Expanded
      : vscode.TreeItemCollapsibleState.Collapsed;
  const node = new CodeOsNode(title, state, children);
  node.description = String(items.length);
  node.tooltip = tooltip;
  node.iconPath = new vscode.ThemeIcon(icon);
  node.contextValue = 'codeos.section';
  return node;
}

function emptyNode(): CodeOsNode {
  const node = new CodeOsNode('(vuoto)', vscode.TreeItemCollapsibleState.None);
  node.iconPath = new vscode.ThemeIcon('dash');
  return node;
}

function invariantNode(inv: LayeringInvariant): CodeOsNode {
  // `downstream ⊀ upstream` = "downstream non deve dipendere da upstream".
  const node = new CodeOsNode(
    `${inv.downstream} ⊀ ${inv.upstream}`,
    vscode.TreeItemCollapsibleState.None,
  );
  const conf = `${(inv.confidence * 100).toFixed(0)}%`;
  const origin = inv.origin === 'declared' ? '📜' : '🔍';
  const cal = inv.calibrated ? ' · tempo' : '';
  node.description = `${origin} sup ${inv.support} · ${conf}${cal}`;
  node.tooltip = new vscode.MarkdownString(
    `**${inv.downstream}** non deve dipendere da **${inv.upstream}**\n\n` +
      `- Supporto: ${inv.support}\n` +
      `- Confidenza: ${conf}${inv.calibrated ? ' (calibrata sul tempo)' : ' (strutturale)'}\n` +
      `- Provenienza: ${inv.origin === 'declared' ? 'dichiarata' : 'scoperta'}`,
  );
  node.iconPath = severityIcon(inv.severity);
  return node;
}

function fossilNode(f: DecisionFossil): CodeOsNode {
  const node = new CodeOsNode(
    `${f.downstream} → ${f.upstream}`,
    vscode.TreeItemCollapsibleState.None,
  );
  const when =
    f.bornAtUnix > 0 ? new Date(f.bornAtUnix * 1000).toLocaleDateString() : 'data ignota';
  const shortHash = f.bornAt ? f.bornAt.slice(0, 8) : '????????';
  node.description = `${shortHash} · ${when}`;
  const structure =
    f.bornStructure.length > 0 ? `\n\n_Diff di nascita:_ ${f.bornStructure.join(', ')}` : '';
  node.tooltip = new vscode.MarkdownString(
    `Confine nato nel commit \`${shortHash}\` (${when})\n\n` +
      `**Intento:** ${f.intent || '_(nessun messaggio di commit)_'}${structure}`,
  );
  node.iconPath = new vscode.ThemeIcon('git-commit');
  return node;
}

function gapNode(g: ArchitecturalGap): CodeOsNode {
  const node = new CodeOsNode(
    `${g.upstream} ✕ ${g.downstream}`,
    vscode.TreeItemCollapsibleState.None,
  );
  node.description = `fondazione ${g.foundationSupport}`;
  node.tooltip = new vscode.MarkdownString(
    `**${g.upstream}** è una fondazione rispettata da ${g.foundationSupport} layer, ` +
      `ma **${g.downstream}** la accoppia in entrambi i versi: l’invariante ` +
      `\`${g.downstream} ⊀ ${g.upstream}\` **manca**. Sospetto debito tecnico o bug.`,
  );
  node.iconPath = severityIcon(g.severity ?? 'warning');
  return node;
}

function violationNode(v: ViolationEvent): CodeOsNode {
  const node = new CodeOsNode(v.message, vscode.TreeItemCollapsibleState.None, [], v.location);
  if (v.location?.filePath) {
    const name = v.location.filePath.split(/[\\/]/).pop() ?? v.location.filePath;
    node.description = `${name}:${v.location.startLine}`;
  }
  node.tooltip = new vscode.MarkdownString(
    `${v.message}\n\n- Regola: \`${v.ruleId}\`\n- Clicca per aprire la posizione.`,
  );
  node.iconPath = severityIcon(v.severity ?? 'high_risk');
  return node;
}

/** Mappa la severità di CodeOS su un'icona a tema (rosso/giallo/azzurro). */
function severityIcon(severity?: string): vscode.ThemeIcon {
  switch (severity) {
    case 'high_risk':
      return new vscode.ThemeIcon('error', new vscode.ThemeColor('charts.red'));
    case 'info':
      return new vscode.ThemeIcon('info', new vscode.ThemeColor('charts.blue'));
    case 'warning':
    default:
      return new vscode.ThemeIcon('warning', new vscode.ThemeColor('charts.yellow'));
  }
}

function violationKey(v: ViolationEvent): string {
  const loc = v.location;
  return `${loc?.filePath ?? ''}:${loc?.startLine ?? 0}:${v.message}`;
}
