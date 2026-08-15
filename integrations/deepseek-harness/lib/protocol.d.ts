export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | {
    readonly [key: string]: JsonValue;
};
export interface CairnSearchHit {
    readonly id: string;
    readonly score: number;
    readonly posterior: number;
    readonly lexicalEvidence: number;
    readonly vectorEvidence: number;
    readonly text: string;
    readonly metadata: Readonly<Record<string, JsonValue>>;
}
export interface CairnSearchResponse {
    readonly revision: number;
    readonly embeddingProvider: string;
    readonly embeddingModel: string;
    readonly dimension: number;
    readonly corpusSha256: string;
    readonly mode: 'cold' | 'warm';
    readonly scoreDomain: 'revision_calibrated_log_odds';
    readonly approximate: boolean;
    readonly hits: readonly CairnSearchHit[];
    readonly remoteBytes: number;
    readonly rangeReads: number;
}
export interface CairnHeadResponse {
    readonly tenant: string;
    readonly knowledgeBase: string;
    readonly revision: number;
    readonly parentRevision?: number;
    readonly createdAtUnixMs: number;
    readonly embeddingProvider: string;
    readonly embeddingModel: string;
    readonly dimension: number;
    readonly shardCount: number;
    readonly hasUqaBundle: boolean;
}
export interface CairnApiError {
    readonly code: string;
    readonly message: string;
    readonly retryable: boolean;
    readonly requestId?: string;
}
export declare function decodeSearchResponse(value: unknown, options: {
    readonly maxHits: number;
    readonly maxMetadataBytesPerHit: number;
}): CairnSearchResponse;
export declare function decodeHeadResponse(value: unknown): CairnHeadResponse;
export declare function decodeApiError(value: unknown, status: number): CairnApiError;
export declare function selectMetadata(source: Readonly<Record<string, JsonValue>>, keys: readonly string[], maxBytes: number): Record<string, JsonValue>;
export declare function errorMessage(error: unknown): string;
