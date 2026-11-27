// Issue #1162: a client with no system prompt about Telekin decides whether to
// save or search from the tool list alone, so the descriptions must say that
// the catalog is where the person's things are remembered.
import { describe, expect, test } from "vitest";

import { useScenario } from "../src/harness.js";
import { SCENARIOS } from "../src/scenarios.js";

describe(SCENARIOS.toolDescriptions.goal, () => {
    const { session } = useScenario(SCENARIOS.toolDescriptions);

    test("search and create_concept describe remembering for the person", async () => {
        const tools = await session().toolDescriptions();

        expect(tools["search"]).toMatch(/what you remember about them/);
        expect(tools["create_concept"]).toMatch(/whenever the person shares something worth keeping/);
        expect(tools["create_concept"]).toMatch(/tell them what you saved/);
        expect(tools["open_changeset"]).toMatch(/saving what the person shares/);
        expect(tools["create_concept"]).not.toMatch(/visible to others/);
    });

    test("get_concept on an index.md names read_document", async () => {
        const error = await session().fail("get_concept", {
            path: "/index.md",
            catalog: session().catalog,
        });

        expect(error.error).toBe("InvalidPath");
        expect(error.message).toContain("read_document");
    });
});
