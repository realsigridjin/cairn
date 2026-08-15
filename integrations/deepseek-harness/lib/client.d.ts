import { type CairnHeadResponse, type CairnSearchResponse, type JsonValue } from './protocol.js';
export interface CairnClientOptions {
    readonly baseUrl: string;
    readonly tenant: string;
    readonly knowledgeBase: string;
    readonly tokenEnv: string;
    readonly timeoutMs: number;
    readonly retries: number;
    readonly maxRetryDelayMs: number;
    readonly maxResponseBytes: number;
    readonly maxHits: number;
    readonly maxMetadataBytesPerHit: number;
}
export interface SearchInput {
    readonly query: string;
    readonly limit: number;
    readonly candidateLimit: number;
    readonly filters: Readonly<Record<string, JsonValue>>;
    readonly callId?: string;
}
export declare class CairnClientError extends Error {
    readonly code: string;
    readonly retryable: boolean;
    readonly status: number | undefined;
    readonly requestId: string | undefined;
    constructor(message: string, options: {
        readonly code: string;
        readonly retryable: boolean;
        readonly status?: number;
        readonly requestId?: string;
        readonly cause?: unknown;
    });
}
export declare function normalizeBaseUrl(raw: string): URL;
export declare function assertCompatibleVersion(version: string, apiVersion: number): void;
export declare class CairnClient {
    #private;
    constructor(options: CairnClientOptions);
    health(signal?: AbortSignal): Promise<{
        readonly version: string;
        readonly apiVersion: number;
    }>;
    head(signal?: AbortSignal): Promise<CairnHeadResponse>;
    search(input: SearchInput, signal?: AbortSignal): Promise<CairnSearchResponse>;
}
