import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { onPageFrozen, resetPageLifecycleForTests } from "./page-lifecycle";

const pageEvent = (type: "pagehide" | "pageshow", persisted: boolean) =>
    Object.assign(new Event(type), { persisted });

describe("page lifecycle", () => {
    let reload: ReturnType<typeof vi.fn>;

    beforeEach(() => {
        reload = vi.fn();
        resetPageLifecycleForTests(reload);
    });
    afterEach(() => resetPageLifecycleForTests());

    it("closes every registered connection when the page enters the cache, and reloads on restore", () => {
        const first = vi.fn();
        const second = vi.fn();
        onPageFrozen(first);
        onPageFrozen(second);

        window.dispatchEvent(pageEvent("pagehide", true));
        expect(first).toHaveBeenCalledTimes(1);
        expect(second).toHaveBeenCalledTimes(1);

        window.dispatchEvent(pageEvent("pageshow", true));
        expect(reload).toHaveBeenCalledTimes(1);
        // A second restore without a new hide does not reload again.
        window.dispatchEvent(pageEvent("pageshow", true));
        expect(reload).toHaveBeenCalledTimes(1);
    });

    it("leaves ordinary unloads and first loads alone", () => {
        const close = vi.fn();
        onPageFrozen(close);
        window.dispatchEvent(pageEvent("pagehide", false));
        window.dispatchEvent(pageEvent("pageshow", false));
        expect(close).not.toHaveBeenCalled();
        expect(reload).not.toHaveBeenCalled();
    });

    it("does not reload a page that had nothing to close", () => {
        const off = onPageFrozen(vi.fn());
        off();
        window.dispatchEvent(pageEvent("pagehide", true));
        window.dispatchEvent(pageEvent("pageshow", true));
        expect(reload).not.toHaveBeenCalled();
    });

    it("keeps closing the others when one close throws", () => {
        const after = vi.fn();
        onPageFrozen(() => {
            throw new Error("boom");
        });
        onPageFrozen(after);
        const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
        window.dispatchEvent(pageEvent("pagehide", true));
        expect(after).toHaveBeenCalledTimes(1);
        warn.mockRestore();
    });
});
