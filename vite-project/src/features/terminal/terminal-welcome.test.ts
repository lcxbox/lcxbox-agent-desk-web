import { describe, expect, it } from "vitest";

import { terminalWelcomeBanner } from "./terminal-welcome";

function visibleLines(banner: string): string[] {
    return banner
        .replace(/\x1b\[[0-9;]*m/g, "")
        .split("\r\n")
        .filter(Boolean);
}

describe("terminalWelcomeBanner", () => {
    it("renders the full LCXBOX Agent Desk artwork when it fits", () => {
        const columns = 76;
        const lines = visibleLines(terminalWelcomeBanner(columns));

        expect(lines).toHaveLength(11);
        expect(lines.some((line) => line.includes("|____/  \\___||___/|_|\\_\\"))).toBe(true);
        expect(Math.max(...lines.map((line) => line.length))).toBeLessThanOrEqual(columns);
    });

    it("uses the compact banner on narrow terminals", () => {
        expect(visibleLines(terminalWelcomeBanner(40))).toEqual([
            "+-------------------+",
            "| LCXBOX Agent Desk |",
            "+-------------------+",
        ]);
    });
});
