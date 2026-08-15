function record(value, label) {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
        throw new Error(`${label} must be an object`);
    }
    return value;
}
function string(value, label, max) {
    if (typeof value !== 'string' || value.length === 0 || value.length > max) {
        throw new Error(`${label} must be a non-empty string of at most ${max} characters`);
    }
    return value;
}
function finite(value, label) {
    if (typeof value !== 'number' || !Number.isFinite(value)) {
        throw new Error(`${label} must be finite`);
    }
    return value;
}
function integer(value, label, min = 0) {
    if (!Number.isSafeInteger(value) || value < min) {
        throw new Error(`${label} must be a safe integer >= ${min}`);
    }
    return value;
}
function boolean(value, label) {
    if (typeof value !== 'boolean')
        throw new Error(`${label} must be boolean`);
    return value;
}
function metadata(value, label, maxBytes) {
    const object = record(value, label);
    const encoded = JSON.stringify(object);
    if (encoded === undefined || new TextEncoder().encode(encoded).byteLength > maxBytes) {
        throw new Error(`${label} exceeds ${maxBytes} bytes`);
    }
    return JSON.parse(encoded);
}
export function decodeSearchResponse(value, options) {
    const root = record(value, 'CAIRN search response');
    const hitsRaw = root.hits;
    if (!Array.isArray(hitsRaw) || hitsRaw.length > options.maxHits) {
        throw new Error(`hits must be an array with at most ${options.maxHits} entries`);
    }
    const seen = new Set();
    let previous = Number.POSITIVE_INFINITY;
    const hits = hitsRaw.map((raw, index) => {
        const hit = record(raw, `hits[${index}]`);
        const id = string(hit.id, `hits[${index}].id`, 4096);
        if (seen.has(id))
            throw new Error(`duplicate hit id: ${id}`);
        seen.add(id);
        const score = finite(hit.score, `hits[${index}].score`);
        if (score > previous)
            throw new Error('hits must be sorted by descending score');
        previous = score;
        const posterior = finite(hit.posterior, `hits[${index}].posterior`);
        if (posterior < 0 || posterior > 1)
            throw new Error('posterior must be in [0,1]');
        return {
            id,
            score,
            posterior,
            lexicalEvidence: finite(hit.lexical_evidence, `hits[${index}].lexical_evidence`),
            vectorEvidence: finite(hit.vector_evidence, `hits[${index}].vector_evidence`),
            text: string(hit.text, `hits[${index}].text`, 16 * 1024 * 1024),
            metadata: metadata(hit.metadata, `hits[${index}].metadata`, options.maxMetadataBytesPerHit),
        };
    });
    const mode = root.mode;
    if (mode !== 'cold' && mode !== 'warm')
        throw new Error('mode must be cold or warm');
    if (root.score_domain !== 'revision_calibrated_log_odds') {
        throw new Error('unsupported score_domain');
    }
    const corpusSha256 = string(root.corpus_sha256, 'corpus_sha256', 64);
    if (!/^[0-9a-f]{64}$/iu.test(corpusSha256))
        throw new Error('invalid corpus_sha256');
    return {
        revision: integer(root.revision, 'revision', 1),
        embeddingProvider: string(root.embedding_provider, 'embedding_provider', 128),
        embeddingModel: string(root.embedding_model, 'embedding_model', 1024),
        dimension: integer(root.dimension, 'dimension', 1),
        corpusSha256,
        mode,
        scoreDomain: 'revision_calibrated_log_odds',
        approximate: boolean(root.approximate, 'approximate'),
        hits,
        remoteBytes: integer(root.remote_bytes, 'remote_bytes'),
        rangeReads: integer(root.range_reads, 'range_reads'),
    };
}
export function decodeHeadResponse(value) {
    const root = record(value, 'CAIRN head response');
    const parent = root.parent_revision === null || root.parent_revision === undefined
        ? undefined
        : integer(root.parent_revision, 'parent_revision', 1);
    return {
        tenant: string(root.tenant, 'tenant', 256),
        knowledgeBase: string(root.knowledge_base, 'knowledge_base', 256),
        revision: integer(root.revision, 'revision', 1),
        ...(parent === undefined ? {} : { parentRevision: parent }),
        createdAtUnixMs: integer(root.created_at_unix_ms, 'created_at_unix_ms'),
        embeddingProvider: string(root.embedding_provider, 'embedding_provider', 128),
        embeddingModel: string(root.embedding_model, 'embedding_model', 1024),
        dimension: integer(root.dimension, 'dimension', 1),
        shardCount: integer(root.shard_count, 'shard_count'),
        hasUqaBundle: boolean(root.has_uqa_bundle, 'has_uqa_bundle'),
    };
}
export function decodeApiError(value, status) {
    try {
        const root = record(value, 'error envelope');
        const error = record(root.error, 'error');
        const requestId = typeof error.request_id === 'string' && error.request_id.length <= 256
            ? error.request_id
            : undefined;
        return {
            code: typeof error.code === 'string' && error.code.length <= 128 ? error.code : `HTTP_${status}`,
            message: typeof error.message === 'string' && error.message.length <= 8192
                ? error.message
                : `CAIRN request failed with HTTP ${status}`,
            retryable: typeof error.retryable === 'boolean' ? error.retryable : status === 429 || status >= 500,
            ...(requestId === undefined ? {} : { requestId }),
        };
    }
    catch {
        return {
            code: `HTTP_${status}`,
            message: `CAIRN request failed with HTTP ${status}`,
            retryable: status === 429 || status >= 500,
        };
    }
}
// Typed handling for sessions-KB chunk metadata. Returns undefined when the
// required continuation fields (session_uid, seq_start) are missing or
// malformed so callers can exclude the segment instead of failing the packet.
// An invalid optional seq_end is dropped on its own; ordering never depends on it.
export function decodeSessionSegmentMetadata(value) {
    const sessionUid = value.session_uid;
    if (typeof sessionUid !== 'string' || sessionUid.length === 0 || sessionUid.length > 256)
        return undefined;
    const seqStart = value.seq_start;
    if (typeof seqStart !== 'number' || !Number.isSafeInteger(seqStart) || seqStart < 0)
        return undefined;
    const seqEnd = value.seq_end;
    if (typeof seqEnd !== 'number' || !Number.isSafeInteger(seqEnd) || seqEnd < seqStart) {
        return { sessionUid, seqStart };
    }
    return { sessionUid, seqStart, seqEnd };
}
export function selectMetadata(source, keys, maxBytes) {
    const output = Object.create(null);
    let used = 2;
    for (const key of keys) {
        if (!Object.hasOwn(source, key))
            continue;
        const value = source[key];
        if (value === undefined)
            continue;
        const bytes = new TextEncoder().encode(JSON.stringify({ [key]: value })).byteLength;
        if (used + bytes > maxBytes)
            continue;
        Object.defineProperty(output, key, { value, enumerable: true });
        used += bytes;
    }
    return output;
}
export function errorMessage(error) {
    return error instanceof Error ? error.message : String(error);
}
