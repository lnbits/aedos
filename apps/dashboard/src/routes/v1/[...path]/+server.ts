import { proxyToOracle } from '$lib/server/oracleProxy';
import type { RequestEvent } from './$types';
import type { RequestHandler } from './$types';

export const DELETE: RequestHandler = proxy;
export const GET: RequestHandler = proxy;
export const PATCH: RequestHandler = proxy;
export const POST: RequestHandler = proxy;
export const PUT: RequestHandler = proxy;

async function proxy(event: RequestEvent) {
  return proxyToOracle(event, `/v1/${event.params.path}`);
}
