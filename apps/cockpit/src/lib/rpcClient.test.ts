import { describe, expect, it, vi } from 'vitest';
import {
  buildRequest,
  coreVersion,
  parseResponse,
  RpcClientError,
  rpcCall,
  type InvokeFn,
} from './rpcClient';

describe('buildRequest', () => {
  it('builds a request with method and a numeric id', () => {
    const request = buildRequest('core.version');
    expect(request.method).toBe('core.version');
    expect(typeof request.id).toBe('number');
  });

  it('assigns increasing ids across calls', () => {
    const first = buildRequest('core.version');
    const second = buildRequest('core.version');
    expect(second.id).toBeGreaterThan(first.id as number);
  });

  it('omits params when none are given', () => {
    const request = buildRequest('core.version');
    expect(JSON.stringify(request)).not.toContain('params');
  });

  it('keeps explicit params, including null', () => {
    expect(buildRequest('m', { a: 1 }).params).toEqual({ a: 1 });
    expect(buildRequest('m', null).params).toBeNull();
  });
});

describe('parseResponse', () => {
  it('parses a success response and returns the payload', () => {
    const response = parseResponse('{"id":1,"result":{"version":"0.1.0"}}');
    expect(response.id).toBe(1);
    expect(response.result).toEqual({ version: '0.1.0' });
    expect(response.error).toBeUndefined();
  });

  it('throws RpcClientError carrying code and message for error responses', () => {
    expect(() =>
      parseResponse('{"id":1,"error":{"code":-32601,"message":"unknown method: nope"}}'),
    ).toThrowError(RpcClientError);
    try {
      parseResponse('{"id":1,"error":{"code":-32601,"message":"unknown method: nope"}}');
    } catch (error) {
      expect(error).toBeInstanceOf(RpcClientError);
      const rpcError = error as RpcClientError;
      expect(rpcError.code).toBe(-32601);
      expect(rpcError.message).toBe('unknown method: nope');
    }
  });
});

describe('rpcCall', () => {
  it('sends a serialized request through the invoke transport and returns the result', async () => {
    const invokeFn: InvokeFn = vi
      .fn()
      .mockResolvedValue(JSON.stringify({ id: 1, result: { version: '0.1.0' } }));

    const result = await rpcCall('core.version', undefined, invokeFn);

    expect(result).toEqual({ version: '0.1.0' });
    expect(invokeFn).toHaveBeenCalledTimes(1);
    const [command, args] = vi.mocked(invokeFn).mock.calls[0];
    expect(command).toBe('rpc');
    const sent = JSON.parse((args as { json: string }).json);
    expect(sent.method).toBe('core.version');
    expect(typeof sent.id).toBe('number');
  });

  it('passes object params through to the wire', async () => {
    const invokeFn: InvokeFn = vi
      .fn()
      .mockResolvedValue(JSON.stringify({ id: 1, result: null }));
    await rpcCall('some.method', { x: 1 }, invokeFn);
    const sent = JSON.parse((vi.mocked(invokeFn).mock.calls[0][1] as { json: string }).json);
    expect(sent.params).toEqual({ x: 1 });
  });

  it('propagates rpc errors as RpcClientError rejections', async () => {
    const invokeFn: InvokeFn = vi.fn().mockResolvedValue(
      JSON.stringify({
        id: 1,
        error: { code: -32602, message: 'params must be an object or null' },
      }),
    );
    await expect(rpcCall('m', [1], invokeFn)).rejects.toMatchObject({
      code: -32602,
      message: 'params must be an object or null',
    });
  });
});

describe('coreVersion', () => {
  it('extracts the version string from core.version', async () => {
    const invokeFn: InvokeFn = vi
      .fn()
      .mockResolvedValue(JSON.stringify({ id: 1, result: { version: '0.1.0' } }));
    await expect(coreVersion(invokeFn)).resolves.toBe('0.1.0');
  });

  it('rejects when the result has no version string', async () => {
    const invokeFn: InvokeFn = vi.fn().mockResolvedValue(JSON.stringify({ id: 1, result: {} }));
    await expect(coreVersion(invokeFn)).rejects.toThrow(/version/);
  });
});
