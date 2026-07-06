import { env } from '$env/dynamic/private';
import type { RequestEvent } from '@sveltejs/kit';

const oracleBase = () => env.ORACLE_BASE_URL || 'http://localhost:8080';

export async function proxyToOracle(event: RequestEvent, targetPath: string) {
  const target = new URL(targetPath, oracleBase());
  target.search = event.url.search;

  const headers = new Headers(event.request.headers);
  headers.delete('host');
  headers.delete('content-length');

  const response = await event.fetch(target, {
    method: event.request.method,
    headers,
    body: event.request.method === 'GET' ? undefined : await event.request.text(),
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
