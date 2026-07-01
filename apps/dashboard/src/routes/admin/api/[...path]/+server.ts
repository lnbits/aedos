import { env } from '$env/dynamic/private';
import type { RequestEvent } from './$types';
import type { RequestHandler } from './$types';

const oracleBase = () => env.ORACLE_BASE_URL || 'http://localhost:8080';

export const GET: RequestHandler = proxy;
export const POST: RequestHandler = proxy;

async function proxy({ params, request, url, fetch }: RequestEvent) {
  const target = new URL(`/admin/api/${params.path}`, oracleBase());
  target.search = url.search;

  const headers = new Headers(request.headers);
  headers.delete('host');
  headers.delete('content-length');

  const response = await fetch(target, {
    method: request.method,
    headers,
    body: request.method === 'GET' ? undefined : await request.text(),
    redirect: 'manual'
  });

  const responseHeaders = new Headers(response.headers);
  responseHeaders.delete('content-encoding');
  responseHeaders.delete('content-length');

  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers: responseHeaders
  });
}
