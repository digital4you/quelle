function (threshold) {
    if (!document.body) return false;
    const hidden = "[hidden], [aria-hidden=true], noscript, template";
    const visible = el => {
        if (el.closest(hidden)) return false;
        for (let p = el; p; p = p.parentElement) {
            if (p.style && (p.style.display === "none" || p.style.visibility === "hidden")) return false;
        }
        return true;
    };
    const text = el => {
        const copy = el.cloneNode(true);
        copy.querySelectorAll("script, style, noscript, template, nav, footer, [role=navigation], [hidden], [aria-hidden=true]")
            .forEach(node => node.remove());
        copy.querySelectorAll("[style]").forEach(node => {
            if (node.style.display === "none" || node.style.visibility === "hidden") node.remove();
        });
        return (copy.textContent || "").replace(/\s+/g, " ").trim();
    };
    const primary = Array.from(document.querySelectorAll("main, [role=main]")).filter(visible);
    const regions = primary.length ? primary : Array.from(document.querySelectorAll("article")).filter(visible);
    const content = regions.length ? regions.map(text).sort((a, b) => b.length - a.length)[0] : text(document.body);
    const pendingTable = Array.from(document.querySelectorAll("[identifier=table], [data-component=table], [role=table]"))
        .some(el => visible(el) && !text(el));
    const pendingFeed = Array.from(document.querySelectorAll("[identifier=teaser-nnsb-container], [data-component=news], [data-component=feed]"))
        .some(el => visible(el) && !el.querySelector("a, article, li, p, td"));
    const loading = /^(loading(?: application| document)?|laden der anwendung|dokument wird geladen|chargement de l[’']application|caricamento dell'applicazione)[.\s…]*$/i.test(content);
    if (pendingTable || pendingFeed || loading || content.length < threshold) {
        delete globalThis.__quelleReadiness;
        return false;
    }
    // Ignore background requests. Require the extracted main content to stop
    // changing briefly, so a heading or a partially populated table is not enough.
    const signature = content;
    const now = Date.now();
    const previous = globalThis.__quelleReadiness;
    if (!previous || previous.signature !== signature) {
        globalThis.__quelleReadiness = {signature, since: now};
        return false;
    }
    // A fully loaded synchronous document needs no hydration grace period.
    // Async network work, pending timers, modules and async scripts retain the
    // original window. Keep a short stability check even on the fast path.
    const synchronous = document.readyState === "complete"
        && globalThis.__quelleActivity && globalThis.__quelleActivity.synchronous()
        && !document.querySelector("script[async], script[type=module]");
    return now - previous.since >= (synchronous ? 80 : 350);
}
