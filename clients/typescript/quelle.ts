/**
 * Zero-dependency TypeScript client for Quelle (fetch + SSE async generators).
 * Works in Node 20+, Deno, Bun, and Edge runtimes.
 */

export type RenderMode = "auto" | "always" | "chromium" | "never";

export type OutputFormat =
  | "markdown"
  | "text"
  | "html"
  | "links"
  | "chunks"
  | "structured"
  | "screenshot"
  | "fullPageScreenshot";

export type PageAction =
  | { type: "click"; selector: string }
  | { type: "wait"; milliseconds: number }
  | { type: "waitForSelector"; selector: string; timeoutMs?: number }
  | { type: "scroll"; direction?: "down" | "up"; pixels?: number; steps?: number }
  | { type: "press"; key: string };

export interface QuelleClientOptions {
  baseUrl?: string;
  token: string;
}

export class QuelleError extends Error {
  readonly status: number;
  readonly body: unknown;

  constructor(status: number, body: unknown) {
    super(`Quelle HTTP ${status}: ${typeof body === "string" ? body : JSON.stringify(body)}`);
    this.status = status;
    this.body = body;
  }
}

export class QuelleClient {
  private readonly baseUrl: string;
  private readonly token: string;

  constructor(options: QuelleClientOptions) {
    if (!options.token) {
      throw new Error("QuelleClient requires a bearer token");
    }
    this.baseUrl = (options.baseUrl ?? "http://127.0.0.1:3000").replace(/\/+$/, "");
    this.token = options.token;
  }

  private headers(accept = "application/json"): Record<string, string> {
    return {
      Authorization: `Bearer ${this.token}`,
      "Content-Type": "application/json",
      Accept: accept,
    };
  }

  private async requestJson<T>(method: string, path: string, body?: unknown): Promise<T> {
    const resp = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: this.headers(),
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await resp.text();
    let parsed: unknown = text;
    try {
      parsed = text ? JSON.parse(text) : {};
    } catch {
      // keep raw text
    }
    if (!resp.ok) {
      throw new QuelleError(resp.status, parsed);
    }
    return parsed as T;
  }

  private async *requestSse<T>(method: string, path: string, body?: unknown): AsyncGenerator<T> {
    const resp = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: this.headers("text/event-stream"),
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!resp.ok || !resp.body) {
      const text = await resp.text();
      throw new QuelleError(resp.status, text);
    }
    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let newlineIdx = buffer.indexOf("\n");
        while (newlineIdx !== -1) {
          const line = buffer.slice(0, newlineIdx).trim();
          buffer = buffer.slice(newlineIdx + 1);
          if (line.startsWith("data:")) {
            const payload = line.slice(5).trim();
            if (payload) {
              yield JSON.parse(payload) as T;
            }
          }
          newlineIdx = buffer.indexOf("\n");
        }
      }
    } finally {
      reader.releaseLock();
    }
  }

  search<T = Record<string, unknown>>(params: {
    query: string;
    language?: "de" | "fr" | "it" | "en";
    limit?: number;
    maxTokens?: number;
    totalMaxTokens?: number;
    focusQuery?: string;
    chunks?: boolean;
    structured?: boolean;
  }): Promise<T> {
    return this.requestJson("POST", "/api/content", params);
  }

  searchStream<T = Record<string, unknown>>(params: {
    query: string;
    language?: "de" | "fr" | "it" | "en";
    limit?: number;
    maxTokens?: number;
    totalMaxTokens?: number;
    focusQuery?: string;
    chunks?: boolean;
    structured?: boolean;
  }): AsyncGenerator<T> {
    return this.requestSse("POST", "/api/content", { ...params, stream: true });
  }

  scrape<T = Record<string, unknown>>(params: {
    url?: string;
    urls?: string[];
    query?: string;
    formats?: OutputFormat[];
    onlyMainContent?: boolean;
    includeTags?: string[];
    excludeTags?: string[];
    render?: RenderMode;
    waitFor?: number;
    waitForSelector?: string;
    scrollSteps?: number;
    actions?: PageAction[];
    maxAge?: number;
    maxTokens?: number;
    chunkTokens?: number;
    focusQuery?: string;
    maxPages?: number;
  }): Promise<T> {
    return this.requestJson("POST", "/api/scrape", params);
  }

  scrapeStream<T = Record<string, unknown>>(params: {
    urls?: string[];
    query?: string;
    formats?: OutputFormat[];
    onlyMainContent?: boolean;
    render?: RenderMode;
    waitForSelector?: string;
    scrollSteps?: number;
    actions?: PageAction[];
  }): AsyncGenerator<T> {
    return this.requestSse("POST", "/api/scrape", { ...params, stream: true });
  }

  extract<T = Record<string, unknown>>(params: {
    url?: string;
    urls?: string[];
    query?: string;
    prompt?: string;
    schema?: Record<string, unknown>;
    render?: RenderMode;
    waitForSelector?: string;
    scrollSteps?: number;
    actions?: PageAction[];
  }): Promise<T> {
    return this.requestJson("POST", "/api/extract", params);
  }

  map<T = Record<string, unknown>>(params: {
    url: string;
    search?: string;
    limit?: number;
    includeSubdomains?: boolean;
    useSitemap?: boolean;
  }): Promise<T> {
    return this.requestJson("POST", "/api/map", params);
  }

  crawl<T = Record<string, unknown>>(params: {
    url: string;
    maxDepth?: number;
    limit?: number;
    sync?: boolean;
    incremental?: boolean;
    sinceCrawlId?: string;
    omitUnchangedContent?: boolean;
    includePaths?: string[];
    excludePaths?: string[];
    actions?: PageAction[];
  }): Promise<T> {
    return this.requestJson("POST", "/api/crawl", params);
  }

  crawlStatus<T = Record<string, unknown>>(crawlId: string): Promise<T> {
    return this.requestJson("GET", `/api/crawl/${encodeURIComponent(crawlId)}`);
  }

  crawlStream<T = Record<string, unknown>>(crawlId: string): AsyncGenerator<T> {
    return this.requestSse("GET", `/api/crawl/${encodeURIComponent(crawlId)}/stream`);
  }

  crawlCancel<T = Record<string, unknown>>(crawlId: string): Promise<T> {
    return this.requestJson("DELETE", `/api/crawl/${encodeURIComponent(crawlId)}`);
  }
}
