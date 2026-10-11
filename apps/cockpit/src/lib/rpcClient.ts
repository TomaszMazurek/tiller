import { invoke } from '@tauri-apps/api/core';

// Wire types mirroring `crates/rpc`. Keep them structurally identical to the
// serde definitions there; the tests pin the JSON shapes on both sides.

export type RequestId = number | string;

export interface RpcRequest {
  id: RequestId;
  method: string;
  params?: unknown;
}

export interface RpcError {
  code: number;
  message: string;
  data?: unknown;
}

export interface RpcResponse {
  id: RequestId;
  result?: unknown;
  error?: RpcError;
}

/** Error raised when the core answers with an `error` response. */
export class RpcClientError extends Error {
  readonly code: number;
  readonly data?: unknown;

  constructor(error: RpcError) {
    super(error.message);
    this.name = 'RpcClientError';
    this.code = error.code;
    this.data = error.data;
  }
}

export type InvokeFn = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

let nextId = 1;

export function buildRequest(method: string, params?: unknown): RpcRequest {
  const request: RpcRequest = { id: nextId++, method };
  if (params !== undefined) {
    request.params = params;
  }
  return request;
}

export function parseResponse(json: string): RpcResponse {
  let parsed: RpcResponse;
  try {
    parsed = JSON.parse(json) as RpcResponse;
  } catch (cause) {
    throw new Error(`malformed rpc response: ${String(json)}`, { cause });
  }
  if (typeof parsed !== 'object' || parsed === null) {
    throw new Error(`malformed rpc response: ${String(json)}`);
  }
  if (parsed.error !== undefined) {
    throw new RpcClientError(parsed.error);
  }
  return parsed;
}

export async function rpcCall(method: string, params?: unknown, invokeFn: InvokeFn = invoke): Promise<unknown> {
  const request = buildRequest(method, params);
  const raw = await invokeFn('rpc', { json: JSON.stringify(request) });
  const response = parseResponse(typeof raw === 'string' ? raw : String(raw));
  return response.result;
}

export async function coreVersion(invokeFn: InvokeFn = invoke): Promise<string> {
  const result = (await rpcCall('core.version', undefined, invokeFn)) as { version?: unknown } | null;
  if (result === null || typeof result !== 'object' || typeof result.version !== 'string') {
    throw new Error('core.version: result is missing a "version" string');
  }
  return result.version;
}
