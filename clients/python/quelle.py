"""Zero-dependency Python client for Quelle (HTTP JSON + SSE streaming)."""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any, Iterator


class QuelleError(RuntimeError):
    """Raised when the Quelle API returns a non-2xx status."""

    def __init__(self, status: int, body: Any) -> None:
        self.status = status
        self.body = body
        super().__init__(f"Quelle HTTP {status}: {body}")


class QuelleClient:
    """Thin, stdlib-only client for all Quelle endpoints."""

    def __init__(
        self,
        base_url: str | None = None,
        token: str | None = None,
        timeout: float = 120.0,
    ) -> None:
        self.base_url = (base_url or os.environ.get("QUELLE_URL") or "http://127.0.0.1:3000").rstrip("/")
        self.token = token or os.environ.get("QUELLE_API_KEY") or os.environ.get("QUELLE_TOKEN") or ""
        if not self.token:
            raise ValueError("QuelleClient requires a bearer token (token arg, QUELLE_API_KEY, or QUELLE_TOKEN)")
        self.timeout = timeout

    def _headers(self, accept: str = "application/json") -> dict[str, str]:
        return {
            "Authorization": f"Bearer {self.token}",
            "Content-Type": "application/json",
            "Accept": accept,
        }

    def _request_json(self, method: str, path: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
        data = json.dumps(payload).encode("utf-8") if payload is not None else None
        req = urllib.request.Request(
            f"{self.base_url}{path}",
            data=data,
            headers=self._headers(),
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read().decode("utf-8")
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as exc:
            raw = exc.read().decode("utf-8", errors="replace")
            try:
                body = json.loads(raw)
            except Exception:
                body = raw
            raise QuelleError(exc.code, body) from exc

    def _request_sse(self, method: str, path: str, payload: dict[str, Any] | None = None) -> Iterator[dict[str, Any]]:
        data = json.dumps(payload).encode("utf-8") if payload is not None else None
        req = urllib.request.Request(
            f"{self.base_url}{path}",
            data=data,
            headers=self._headers(accept="text/event-stream"),
            method=method,
        )
        try:
            resp = urllib.request.urlopen(req, timeout=self.timeout)
        except urllib.error.HTTPError as exc:
            raw = exc.read().decode("utf-8", errors="replace")
            try:
                body = json.loads(raw)
            except Exception:
                body = raw
            raise QuelleError(exc.code, body) from exc

        with resp:
            for raw_line in resp:
                line = raw_line.decode("utf-8", errors="replace").strip()
                if not line.startswith("data:"):
                    continue
                payload_str = line[5:].strip()
                if not payload_str:
                    continue
                yield json.loads(payload_str)

    def search(self, query: str, **kwargs: Any) -> dict[str, Any]:
        """POST /api/content (parallel RRF meta-search + extraction)."""
        return self._request_json("POST", "/api/content", {"query": query, **kwargs})

    def search_stream(self, query: str, **kwargs: Any) -> Iterator[dict[str, Any]]:
        """POST /api/content with stream=true (yields source/error/done SSE events)."""
        return self._request_sse("POST", "/api/content", {"query": query, **kwargs, "stream": True})

    def scrape(
        self,
        url: str | None = None,
        *,
        urls: list[str] | None = None,
        query: str | None = None,
        **kwargs: Any,
    ) -> dict[str, Any]:
        """POST /api/scrape (single URL, batch URLs, or query)."""
        body: dict[str, Any] = {**kwargs}
        if url is not None:
            body["url"] = url
        if urls is not None:
            body["urls"] = urls
        if query is not None:
            body["query"] = query
        return self._request_json("POST", "/api/scrape", body)

    def scrape_stream(
        self,
        *,
        urls: list[str] | None = None,
        query: str | None = None,
        **kwargs: Any,
    ) -> Iterator[dict[str, Any]]:
        """POST /api/scrape with stream=true for batch/query SSE streaming."""
        body: dict[str, Any] = {**kwargs, "stream": True}
        if urls is not None:
            body["urls"] = urls
        if query is not None:
            body["query"] = query
        return self._request_sse("POST", "/api/scrape", body)

    def extract(
        self,
        *,
        url: str | None = None,
        urls: list[str] | None = None,
        query: str | None = None,
        prompt: str | None = None,
        schema: dict[str, Any] | None = None,
        **kwargs: Any,
    ) -> dict[str, Any]:
        """POST /api/extract (grounded LLM JSON/schema extraction)."""
        body: dict[str, Any] = {**kwargs}
        if url is not None:
            body["url"] = url
        if urls is not None:
            body["urls"] = urls
        if query is not None:
            body["query"] = query
        if prompt is not None:
            body["prompt"] = prompt
        if schema is not None:
            body["schema"] = schema
        return self._request_json("POST", "/api/extract", body)

    def map(self, url: str, **kwargs: Any) -> dict[str, Any]:
        """POST /api/map (fast URL discovery)."""
        return self._request_json("POST", "/api/map", {"url": url, **kwargs})

    def crawl(self, url: str, **kwargs: Any) -> dict[str, Any]:
        """POST /api/crawl (sync or async recursive crawl)."""
        return self._request_json("POST", "/api/crawl", {"url": url, **kwargs})

    def crawl_status(self, crawl_id: str) -> dict[str, Any]:
        """GET /api/crawl/{id}."""
        return self._request_json("GET", f"/api/crawl/{crawl_id}")

    def crawl_stream(self, crawl_id: str) -> Iterator[dict[str, Any]]:
        """GET /api/crawl/{id}/stream (live SSE crawl events)."""
        return self._request_sse("GET", f"/api/crawl/{crawl_id}/stream")

    def crawl_cancel(self, crawl_id: str) -> dict[str, Any]:
        """DELETE /api/crawl/{id}."""
        return self._request_json("DELETE", f"/api/crawl/{crawl_id}")
