import { decodeApiError, decodeHeadResponse, decodeSearchResponse, } from './protocol.js';
export class CairnClientError extends Error {
    code;
    retryable;
    status;
    requestId;
    constructor(message, options) {
        super(message, { cause: options.cause });
        this.name = 'CairnClientError';
        this.code = options.code;
        this.retryable = options.retryable;
        this.status = options.status;
        this.requestId = options.requestId;
    }
}
const SAFE_COMPONENT = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u;
const SAFE_CALL_ID = /^[A-Za-z0-9._:-]{1,256}$/u;
const JSON_CONTENT_TYPE = /^(application\/json|[^;]+\+json)(?:;|$)/iu;
const RETRYABLE_STATUS = new Set([408, 425, 429, 500, 502, 503, 504]);
export function normalizeBaseUrl(raw) {
    const parsed = new URL(raw.endsWith('/') ? raw : `${raw}/`);
    const host = parsed.hostname;
    const localHttp = parsed.protocol === 'http:'
        && (host === 'localhost' || host === '127.0.0.1' || host === '[::1]' || host === '::1');
    if (parsed.protocol !== 'https:' && !localHttp) {
        throw new Error('CAIRN baseUrl must use HTTPS; plain HTTP is allowed only for exact loopback hosts');
    }
    if (parsed.username.length > 0 || parsed.password.length > 0) {
        throw new Error('CAIRN baseUrl must not contain credentials');
    }
    if (parsed.search.length > 0 || parsed.hash.length > 0) {
        throw new Error('CAIRN baseUrl must not contain a query string or fragment');
    }
    const decodedPath = decodeURIComponent(parsed.pathname);
    if (decodedPath.split('/').some(part => part === '..') || /%2f|%5c/iu.test(parsed.pathname)) {
        throw new Error('CAIRN baseUrl contains an unsafe path');
    }
    return parsed;
}
export function assertCompatibleVersion(version, apiVersion) {
    const match = /^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/u.exec(version);
    if (match === null || Number(match[1]) !== 1) {
        throw new Error(`unsupported CAIRN version ${version}; expected >=1.0.0 <2.0.0`);
    }
    if (apiVersion !== 1)
        throw new Error(`unsupported CAIRN HTTP API version ${apiVersion}`);
}
export class CairnClient {
    #options;
    #base;
    constructor(options) {
        if (!SAFE_COMPONENT.test(options.tenant) || !SAFE_COMPONENT.test(options.knowledgeBase)) {
            throw new Error('tenant and knowledgeBase must be safe CAIRN scope components');
        }
        if (!Number.isSafeInteger(options.timeoutMs) || options.timeoutMs < 1)
            throw new Error('invalid timeoutMs');
        if (!Number.isSafeInteger(options.retries) || options.retries < 0 || options.retries > 5)
            throw new Error('invalid retries');
        this.#options = options;
        this.#base = normalizeBaseUrl(options.baseUrl);
    }
    async health(signal) {
        const value = await this.#request('health', { method: 'GET' }, signal);
        const root = asRecord(value);
        const version = typeof root.version === 'string' ? root.version : '';
        const apiVersion = Number(root.api_version);
        assertCompatibleVersion(version, apiVersion);
        return { version, apiVersion };
    }
    async head(signal) {
        const value = await this.#request(this.#kbPath('head'), { method: 'GET' }, signal);
        const head = decodeHeadResponse(value);
        if (head.tenant !== this.#options.tenant || head.knowledgeBase !== this.#options.knowledgeBase) {
            throw new Error('CAIRN head scope does not match trusted plugin configuration');
        }
        return head;
    }
    async search(input, signal) {
        const value = await this.#request(this.#kbPath('search'), {
            method: 'POST',
            headers: {
                'content-type': 'application/json',
                ...(input.callId !== undefined && SAFE_CALL_ID.test(input.callId)
                    ? { 'x-dsh-tool-call-id': input.callId }
                    : {}),
            },
            body: JSON.stringify({
                query: input.query,
                limit: input.limit,
                candidate_limit: input.candidateLimit,
                filters: input.filters,
            }),
        }, signal);
        return decodeSearchResponse(value, {
            maxHits: input.limit,
            maxMetadataBytesPerHit: this.#options.maxMetadataBytesPerHit,
        });
    }
    #kbPath(action) {
        return `v1/${encodeURIComponent(this.#options.tenant)}/kb/${encodeURIComponent(this.#options.knowledgeBase)}/${action}`;
    }
    async #request(path, init, parentSignal) {
        const deadline = new AbortController();
        const onAbort = () => deadline.abort(parentSignal?.reason);
        parentSignal?.addEventListener('abort', onAbort, { once: true });
        const timer = setTimeout(() => deadline.abort(new Error('CAIRN request deadline exceeded')), this.#options.timeoutMs);
        try {
            let attempt = 0;
            while (true) {
                try {
                    const response = await fetch(new URL(path, this.#base), {
                        ...init,
                        redirect: 'error',
                        signal: deadline.signal,
                        headers: {
                            accept: 'application/json',
                            ...this.#authHeaders(),
                            ...(init.headers ?? {}),
                        },
                    });
                    const requestId = response.headers.get('x-request-id') ?? undefined;
                    const contentType = response.headers.get('content-type') ?? '';
                    const bytes = await readBoundedBody(response, this.#options.maxResponseBytes, deadline.signal);
                    const parsed = parseJson(bytes, contentType);
                    if (response.ok)
                        return parsed;
                    const apiError = decodeApiError(parsed, response.status);
                    const shouldRetry = attempt < this.#options.retries
                        && RETRYABLE_STATUS.has(response.status)
                        && apiError.retryable;
                    if (shouldRetry) {
                        await sleep(this.#retryDelay(response, attempt), deadline.signal);
                        attempt += 1;
                        continue;
                    }
                    const errorRequestId = apiError.requestId ?? requestId;
                    throw new CairnClientError(apiError.message, {
                        code: apiError.code,
                        retryable: apiError.retryable,
                        status: response.status,
                        ...(errorRequestId === undefined ? {} : { requestId: errorRequestId }),
                    });
                }
                catch (error) {
                    if (deadline.signal.aborted)
                        throw deadline.signal.reason ?? error;
                    if (error instanceof CairnClientError)
                        throw error;
                    if (attempt >= this.#options.retries) {
                        throw new CairnClientError(`CAIRN transport failed: ${message(error)}`, {
                            code: 'TRANSPORT_ERROR',
                            retryable: true,
                            cause: error,
                        });
                    }
                    await sleep(this.#retryDelay(undefined, attempt), deadline.signal);
                    attempt += 1;
                }
            }
        }
        finally {
            clearTimeout(timer);
            parentSignal?.removeEventListener('abort', onAbort);
        }
    }
    #authHeaders() {
        if (this.#options.tokenEnv.length === 0)
            return {};
        const token = process.env[this.#options.tokenEnv]?.trim();
        if (token === undefined || token.length === 0)
            return {};
        if (token.length < 16 || token.length > 4096 || !/^[!-~]+$/u.test(token)) {
            throw new Error(`${this.#options.tokenEnv} must contain 16..=4096 visible ASCII bytes`);
        }
        return { authorization: `Bearer ${token}` };
    }
    #retryDelay(response, attempt) {
        const retryAfter = response?.headers.get('retry-after');
        if (retryAfter !== null && retryAfter !== undefined) {
            const seconds = Number(retryAfter);
            if (Number.isFinite(seconds) && seconds >= 0) {
                return Math.min(this.#options.maxRetryDelayMs, Math.round(seconds * 1000));
            }
        }
        return Math.min(this.#options.maxRetryDelayMs, 100 * (2 ** attempt));
    }
}
async function readBoundedBody(response, maxBytes, signal) {
    const declared = Number(response.headers.get('content-length'));
    if (Number.isFinite(declared) && declared > maxBytes)
        throw new Error(`CAIRN response exceeds ${maxBytes} bytes`);
    if (response.body === null)
        return new Uint8Array();
    const reader = response.body.getReader();
    const chunks = [];
    let total = 0;
    try {
        while (true) {
            if (signal.aborted)
                throw signal.reason;
            const { done, value } = await reader.read();
            if (done)
                break;
            total += value.byteLength;
            if (total > maxBytes)
                throw new Error(`CAIRN response exceeds ${maxBytes} bytes`);
            chunks.push(value);
        }
    }
    finally {
        reader.releaseLock();
    }
    const output = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
        output.set(chunk, offset);
        offset += chunk.byteLength;
    }
    return output;
}
function parseJson(bytes, contentType) {
    if (!JSON_CONTENT_TYPE.test(contentType))
        throw new Error(`CAIRN returned non-JSON content-type: ${contentType || '(missing)'}`);
    const text = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    return JSON.parse(text);
}
async function sleep(ms, signal) {
    if (ms <= 0)
        return;
    await new Promise((resolve, reject) => {
        const timer = setTimeout(resolve, ms);
        const abort = () => {
            clearTimeout(timer);
            reject(signal.reason);
        };
        signal.addEventListener('abort', abort, { once: true });
    });
}
function asRecord(value) {
    if (value === null || typeof value !== 'object' || Array.isArray(value))
        throw new Error('CAIRN response must be an object');
    return value;
}
function message(error) {
    return error instanceof Error ? error.message : String(error);
}
