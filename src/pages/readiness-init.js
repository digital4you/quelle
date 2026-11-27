(() => {
    "use strict";
    // This only selects a shorter stability window or extends an adaptive
    // readiness wait while timers/network/DOM work is still pending. Failure
    // to observe activity keeps the conservative window; it never makes a
    // document pass validation.
    const timeouts = new Set();
    const intervals = new Set();
    let asynchronous = false;
    let observed = true;
    let inFlight = 0;
    let lastMutationAt = 0;
    let observerAttached = false;

    const attachObserver = () => {
        if (observerAttached || typeof MutationObserver !== "function") return;
        const root = document.documentElement || document.body;
        if (!root) return;
        try {
            const mo = new MutationObserver(() => {
                lastMutationAt = Date.now();
            });
            mo.observe(root, { childList: true, subtree: true, characterData: true });
            observerAttached = true;
        } catch (_) {}
    };

    try {
        for (const [name, set, repeating] of [
            ["setTimeout", timeouts, false],
            ["setInterval", intervals, true],
        ]) {
            const original = globalThis[name];
            globalThis[name] = function (...args) {
                const callback = args[0];
                if (typeof callback !== "function" || set.size >= 1024) {
                    asynchronous = true;
                    return Reflect.apply(original, this, args);
                }
                let id;
                args[0] = function (...values) {
                    try {
                        return Reflect.apply(callback, this, values);
                    } finally {
                        if (!repeating) set.delete(id);
                    }
                };
                id = Reflect.apply(original, this, args);
                set.add(id);
                return id;
            };
        }
        for (const [name, set] of [
            ["clearTimeout", timeouts],
            ["clearInterval", intervals],
        ]) {
            const original = globalThis[name];
            globalThis[name] = function (...args) {
                set.delete(args[0]);
                return Reflect.apply(original, this, args);
            };
        }

        const origFetch = globalThis.fetch;
        if (typeof origFetch === "function") {
            globalThis.fetch = function (...args) {
                asynchronous = true;
                inFlight += 1;
                const dec = () => {
                    inFlight = Math.max(0, inFlight - 1);
                };
                try {
                    const res = Reflect.apply(origFetch, this, args);
                    if (res && typeof res.then === "function") {
                        res.then(dec, dec);
                    } else {
                        dec();
                    }
                    return res;
                } catch (err) {
                    dec();
                    throw err;
                }
            };
        }

        const xhrProto = globalThis.XMLHttpRequest && XMLHttpRequest.prototype;
        if (xhrProto && typeof xhrProto.send === "function") {
            const origSend = xhrProto.send;
            xhrProto.send = function (...args) {
                asynchronous = true;
                inFlight += 1;
                const dec = () => {
                    inFlight = Math.max(0, inFlight - 1);
                };
                try {
                    if (typeof this.addEventListener === "function") {
                        this.addEventListener("loadend", dec, { once: true });
                    }
                    return Reflect.apply(origSend, this, args);
                } catch (err) {
                    dec();
                    throw err;
                }
            };
        }

        const markAsync = (owner, name) => {
            const original = owner && owner[name];
            if (typeof original !== "function") return;
            owner[name] = function (...args) {
                asynchronous = true;
                return Reflect.apply(original, this, args);
            };
        };
        markAsync(globalThis, "requestAnimationFrame");
        markAsync(globalThis, "requestIdleCallback");
        for (const name of ["WebSocket", "EventSource", "Worker", "SharedWorker"]) {
            if (typeof globalThis[name] !== "function") continue;
            globalThis[name] = new Proxy(globalThis[name], {
                construct(target, args, newTarget) {
                    asynchronous = true;
                    return Reflect.construct(target, args, newTarget);
                },
            });
        }
        if (typeof document !== "undefined" && typeof document.addEventListener === "function") {
            document.addEventListener("DOMContentLoaded", attachObserver, { once: true });
        }
        attachObserver();
    } catch (_) {
        observed = false;
    }

    globalThis.__quelleActivity = Object.freeze({
        synchronous: () =>
            observed && !asynchronous && timeouts.size === 0 && intervals.size === 0,
        busy: () => {
            attachObserver();
            return (
                timeouts.size > 0
                || inFlight > 0
                || (lastMutationAt > 0 && Date.now() - lastMutationAt < 1500)
            );
        },
    });
})();
