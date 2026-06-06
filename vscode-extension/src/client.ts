// Sottile wrapper attorno al servizio gRPC `codeos.v1.CodeOs`.
//
// Carica il `.proto` a runtime con `@grpc/proto-loader` (niente codegen, niente
// dipendenze proprietarie) e mappa metodi unari su `Promise` e lo stream
// `WatchEvents` su una callback tipizzata. Lato JS i campi snake_case del proto
// diventano camelCase (`keepCase: false`).

import * as grpc from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';

export interface SourceLocation {
  filePath: string;
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
}

export interface Entity {
  id: string;
  kind: string;
  qualifiedName: string;
  location?: SourceLocation;
  metadata: Record<string, string>;
}

export interface Relation {
  id: string;
  kind: string;
  sourceId: string;
  targetId: string;
  metadata: Record<string, string>;
}

export interface QueryResult {
  formattedContext: string;
  entities: Entity[];
  relations: Relation[];
}

export interface ViolationEvent {
  ruleId: string;
  relationId: string;
  sourceId: string;
  targetId: string;
  message: string;
  /** Dove vive la dipendenza proibita (entità sorgente). Assente se non nota. */
  location?: SourceLocation;
  /** Gravità: "info" | "warning" | "high_risk" (una violazione attiva è high_risk). */
  severity?: string;
}

export type CodeOsEvent =
  | { type: 'filesIndexed'; filePaths: string[] }
  | {
      type: 'graphUpdated';
      addedEntities: number;
      removedEntities: number;
      addedRelations: number;
      removedRelations: number;
    }
  | { type: 'violation'; violation: ViolationEvent }
  | {
      type: 'indexProgress';
      totalFiles: number;
      processedFiles: number;
      currentFile: string;
      skippedFiles: number;
      parseErrors: number;
    };

export interface RecordDecisionInput {
  author: string;
  title: string;
  context: string;
  rationale: string;
  relatedEntityIds?: string[];
  relatedDecisionIds?: string[];
  tags?: string[];
}

/** Un invariante di layering scoperto (asse struttura, calibrato sul tempo). */
export interface LayeringInvariant {
  upstream: string;
  downstream: string;
  support: number;
  /** Confidenza in [0,1]. Dal Campo di Astensione se `calibrated`, altrimenti strutturale. */
  confidence: number;
  calibrated: boolean;
  /** Provenienza: "discovered" (dedotta dal grafo) | "declared" (config a mano). */
  origin?: string;
  /** Gravità: "info" | "warning" | "high_risk" (derivata dalla confidenza). */
  severity?: string;
}

/** Un invariante in formazione (stadio 1): la stessa asimmetria pura di un
 *  invariante, ma ancora sotto soglia. Derivato e mai persistito. Niente
 *  confidence/severity: un confine non ancora formato non si stima (un segnale,
 *  non una verità). `needed` dice quanti archi mancano alla promozione. */
export interface LayeringCandidate {
  upstream: string;
  downstream: string;
  support: number;
  needed: number;
}

/** La nascita storica di un confine (Fossile di Decisione, asse intento). */
export interface DecisionFossil {
  upstream: string;
  downstream: string;
  bornAt: string;
  bornAtUnix: number;
  intent: string;
  bornStructure: string[];
}

/** Una lacuna del secondo ordine: l'invariante che manca (asse meta). */
export interface ArchitecturalGap {
  upstream: string;
  downstream: string;
  foundationSupport: number;
  /** Gravità: "info" | "warning" | "high_risk" (derivata dal supporto della fondazione). */
  severity?: string;
}

/** Il referto architetturale completo: lo spazio negativo lungo i quattro assi. */
export interface ArchitectureReport {
  invariants: LayeringInvariant[];
  candidates: LayeringCandidate[];
  fossils: DecisionFossil[];
  gaps: ArchitecturalGap[];
}

export interface GuardBeforeResponse {
  targetFiles: string[];
  boundaries: string[];
  blastRadius: number;
  safePath: string;
  contextPack: string;
}

export interface GuardAfterResponse {
  newRelations: string[];
  violations: ViolationEvent[];
  proposedFixes: string[];
}

export interface GetContextPackResponse {
  goalInterpretation: string;
  filesToRead: string[];
  relevantEntities: string[];
  keyDependencies: string[];
  boundariesToPreserve: string[];
  localPatterns: string[];
  suggestedTests: string[];
  estimatedRisk: string;
  formattedMarkdown: string;
}

export interface PrMriResponse {
  newDependencies: string[];
  violatedBoundaries: string[];
  blastRadiusChange: number;
  historicalHotspots: string[];
  newExternalDependencies: string[];
  impactedTests: string[];
  riskScore: string;
  summary: string;
}

export interface SimulateResponse {
  dependenciesToRewrite: string[];
  changedBoundaries: string[];
  risks: string[];
  suggestedTests: string[];
  recommendationPlan: string[];
}

export interface WhyResponse {
  bornCommit: string;
  bornDate: string;
  intent: string;
  coChangedFiles: string[];
  markdownDecisions: string[];
  explanation: string;
  historyInsufficient: boolean;
}

interface WatchHandlers {
  onEvent: (event: CodeOsEvent) => void;
  onError: (error: Error) => void;
  onEnd: () => void;
}

/** Client del servizio `CodeOs`. Una istanza = una connessione gRPC. */
export class CodeOsClient {
  private readonly client: grpc.Client & Record<string, any>;

  constructor(address: string, protoPath: string) {
    const packageDef = protoLoader.loadSync(protoPath, {
      keepCase: false,
      longs: String,
      enums: String,
      defaults: true,
      oneofs: true,
    });
    const proto = grpc.loadPackageDefinition(packageDef) as any;
    const ServiceCtor = proto.codeos.v1.CodeOs;
    this.client = new ServiceCtor(address, grpc.credentials.createInsecure());
  }

  queryGraph(naturalLanguage: string): Promise<QueryResult> {
    return this.unary<QueryResult>('QueryGraph', { naturalLanguage });
  }

  indexProject(projectRoot: string): Promise<void> {
    return this.unary<void>('IndexProject', { projectRoot });
  }

  async indexFiles(files: string[]): Promise<string[]> {
    const resp = await this.unary<{ entityIds: string[] }>('IndexFiles', { files });
    return resp.entityIds ?? [];
  }

  async recordDecision(input: RecordDecisionInput): Promise<string> {
    const resp = await this.unary<{ decisionId: string }>('RecordDecision', {
      author: input.author,
      title: input.title,
      context: input.context,
      rationale: input.rationale,
      relatedEntityIds: input.relatedEntityIds ?? [],
      relatedDecisionIds: input.relatedDecisionIds ?? [],
      tags: input.tags ?? [],
    });
    return resp.decisionId;
  }

  /** Chiede il referto architetturale: lo spazio negativo lungo i quattro assi. */
  async getArchitectureReport(): Promise<ArchitectureReport> {
    const resp = await this.unary<Partial<ArchitectureReport>>(
      'GetArchitectureReport',
      {},
    );
    return {
      invariants: resp.invariants ?? [],
      candidates: resp.candidates ?? [],
      fossils: resp.fossils ?? [],
      gaps: resp.gaps ?? [],
    };
  }

  guardBefore(goal: string): Promise<GuardBeforeResponse> {
    return this.unary<GuardBeforeResponse>('GuardBefore', { goal });
  }

  guardAfter(): Promise<GuardAfterResponse> {
    return this.unary<GuardAfterResponse>('GuardAfter', {});
  }

  getContextPack(goal: string, forAi: boolean): Promise<GetContextPackResponse> {
    return this.unary<GetContextPackResponse>('GetContextPack', { goal, forAi });
  }

  prMri(base: string, head: string): Promise<PrMriResponse> {
    return this.unary<PrMriResponse>('PrMri', { base, head });
  }

  simulate(expr: string): Promise<SimulateResponse> {
    return this.unary<SimulateResponse>('Simulate', { expr });
  }

  why(expr: string): Promise<WhyResponse> {
    return this.unary<WhyResponse>('Why', { expr });
  }

  /** Apre lo stream server `WatchEvents`. Restituisce una funzione per chiuderlo. */
  watchEvents(handlers: WatchHandlers): () => void {
    const stream: grpc.ClientReadableStream<any> = this.client.WatchEvents({});
    stream.on('data', (msg: any) => {
      const decoded = decodeEvent(msg);
      if (decoded) {
        handlers.onEvent(decoded);
      }
    });
    stream.on('error', (err: Error) => handlers.onError(err));
    stream.on('end', () => handlers.onEnd());
    return () => stream.cancel();
  }

  close(): void {
    this.client.close();
  }

  private unary<T>(method: string, request: unknown): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      this.client[method](request, (err: grpc.ServiceError | null, resp: T) => {
        if (err) {
          reject(err);
        } else {
          resolve(resp);
        }
      });
    });
  }
}

/** Traduce un `EventMessage` (oneof) nel tipo discriminato `CodeOsEvent`. */
function decodeEvent(msg: any): CodeOsEvent | undefined {
  switch (msg.event) {
    case 'filesIndexed':
      return { type: 'filesIndexed', filePaths: msg.filesIndexed?.filePaths ?? [] };
    case 'graphUpdated':
      return {
        type: 'graphUpdated',
        addedEntities: Number(msg.graphUpdated?.addedEntities ?? 0),
        removedEntities: Number(msg.graphUpdated?.removedEntities ?? 0),
        addedRelations: Number(msg.graphUpdated?.addedRelations ?? 0),
        removedRelations: Number(msg.graphUpdated?.removedRelations ?? 0),
      };
    case 'violation':
      return { type: 'violation', violation: msg.violation as ViolationEvent };
    case 'indexProgress':
      return {
        type: 'indexProgress',
        totalFiles: Number(msg.indexProgress?.totalFiles ?? 0),
        processedFiles: Number(msg.indexProgress?.processedFiles ?? 0),
        currentFile: String(msg.indexProgress?.currentFile ?? ''),
        skippedFiles: Number(msg.indexProgress?.skippedFiles ?? 0),
        parseErrors: Number(msg.indexProgress?.parseErrors ?? 0),
      };
    default:
      return undefined;
  }
}
