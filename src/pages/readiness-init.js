(() => {
    "use strict";
    // This only selects a shorter stability window. Failure to observe activity
    // keeps the conservative window; it never makes a document pass validation.
    const timers = new Set();
    let asynchronous = false;
    let observed = true;
    try {
        for (const [name, repeating] of [["setTimeout", false], ["setInterval", true]]) {
            const original = globalThis[name];
            globalThis[name] = function (...args) {
                const callback = args[0];
                if (typeof callback !== "function" || timers.size >= 1024) {
                    asynchronous = true;
                    return Reflect.apply(original, this, args);
                }
                let id;
                args[0] = function (...values) {
                    try {
                        return Reflect.apply(callback, this, values);
                    } finally {
                        if (!repeating) timers.delete(id);
                    }
                };
                id = Reflect.apply(original, this, args);
                timers.add(id);
                return id;
            };
        }
        for (const name of ["clearTimeout", "clearInterval"]) {
            const original = globalThis[name];
            globalThis[name] = function (...args) {
                timers.delete(args[0]);
                return Reflect.apply(original, this, args);
            };
        }
        const markAsync = (owner, name) => {
            const original = owner && owner[name];
            if (typeof original !== "function") return;
            owner[name] = function (...args) {
                asynchronous = true;
                // Return the original object/promise, without reading a body
                // or adding promise handlers that could change page behavior.
                return Reflect.apply(original, this, args);
            };
        };
        markAsync(globalThis, "fetch");
        markAsync(globalThis.XMLHttpRequest && XMLHttpRequest.prototype, "send");
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
    } catch (_) {
        observed = false;
    }
    globalThis.__quelleActivity = Object.freeze({
        synchronous: () => observed && !asynchronous && timers.size === 0,
    });
})();
