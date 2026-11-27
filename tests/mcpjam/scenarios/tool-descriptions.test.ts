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

    // Issue #1174: a client that defers tools shows the model only `name - <first 31
    // characters>` until it loads one. This is the deferred view, built from the live tool list.
    // It checks what the model would see; no model runs here, so whether a model then reaches a
    // write tool is a manual run in the Claude app (see the PR).
    test("the deferred view of the tools front-loads the save and recall path", async () => {
        const tools = await session().toolDescriptions();
        const deferred = Object.fromEntries(
            Object.entries(tools).map(([name, description]) => [name, description.slice(0, 31)]),
        );

        expect(deferred["create_concept"]).toBe("Save what the person wants kept");
        expect(deferred["search"]).toBe("Recall what the person told you");
        expect(deferred["open_changeset"]).toBe("Step 1 of saving to memory. Ope");
        expect(deferred["publish"]).toBe("Final step of saving to memory.");
        expect(deferred["guide"]).toMatch(/^Read first: how to use /);
        // Nothing opens with code or a parameter name: the label says what the tool is for.
        for (const [name, label] of Object.entries(deferred)) {
            expect(label, name).not.toMatch(/[`_{]/);
        }
    });

    test("guide hands a client that drops instructions the same save-by-default guidance", async () => {
        const guide = await session().call<{ instructions: string }>("guide", {});

        expect(guide.instructions).toMatch(/save it to the catalog/);
        expect(guide.instructions).toMatch(/`search` the catalog first/);
        expect(guide.instructions).toMatch(/call `open_changeset` first/);
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
