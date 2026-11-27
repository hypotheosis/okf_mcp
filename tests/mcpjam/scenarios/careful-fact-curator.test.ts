// Persona "Careful Fact Curator": sourcing, drafts, folders and stamps.
import { describe, expect, test, vi } from "vitest";

import { create, remember, useScenario } from "../src/harness.js";
import type { SourceEntry, Stamp } from "../src/okf.js";
import { SCENARIOS } from "../src/scenarios.js";

describe(SCENARIOS.createAndSource.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.createAndSource);

    test("a concept created and sourced in one changeset publishes with its source", async () => {
        const path = paths.at("history/first-bike");
        const changeset = await session().openChangeset();
        await create(session(), changeset, {
            path,
            title: "First bike",
            body: "# First bike\n\nI got my first bike, a red one, in 1988.\n",
            summary: "My first bike, a red one, in 1988.",
        });
        // Sequential, after create_concept returned: the draft is visible to
        // the next write in the same changeset (the swarm's NotFound came from
        // sending both at once).
        await session().call("add_source", {
            path,
            id: "photo-album-1988",
            resource: "Family photo album, 1988",
            title: "Photo album",
            author: "human:tester",
            changeset,
        });
        await session().publish(changeset, "Remember my first bike, with its source");

        const concept = await session().concept(path);
        expect(concept.frontmatter.sources).toEqual([
            {
                id: "photo-album-1988",
                resource: "Family photo album, 1988",
                title: "Photo album",
                author: "human:tester",
            },
        ]);
    });

    // Issue #929: a taken path is its own error, so a client that branches on `error` knows to
    // edit the concept instead.
    test("create_concept on an existing path is AlreadyExists, naming the edit tools", async () => {
        const path = paths.at("history/second-bike");
        const changeset = await session().openChangeset();
        const fields = { path, title: "Second bike", body: "# Second bike\n\nBlue.\n", summary: "Blue." };
        await create(session(), changeset, fields);
        const error = await session().fail("create_concept", {
            path,
            frontmatter: { type: "note", title: "Second bike" },
            body: "# Second bike\n\nGreen.\n",
            changeset,
        });
        expect(error.error).toBe("AlreadyExists");
        expect(String(error.path)).toContain("history/second-bike");
        expect(error.message).toContain("edit_document");
        expect(error.message).toContain("replace_body");
        await session().call("discard", { changeset });
    });

    test("a source on a concept that does not exist is NotFound, naming the path", async () => {
        const path = paths.at("history/never-written");
        const changeset = await session().openChangeset();
        const error = await session().fail("add_source", {
            path,
            id: "nothing",
            resource: "nowhere",
            changeset,
        });
        expect(error.error).toBe("NotFound");
        expect(error.message).toContain(path);
        await session().call("discard", { changeset });
    });
});

describe(SCENARIOS.reviewAndPublish.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.reviewAndPublish);

    test("a draft is visible through its changeset only, until publish", async () => {
        const path = paths.at("garden-plan");
        const changeset = await session().openChangeset();
        await create(session(), changeset, {
            path,
            title: "Garden plan",
            body: "# Garden plan\n\nTomatoes go along the south fence.\n",
        });

        const draft = await session().concept(path, { changeset });
        expect(draft.body).toContain("Tomatoes go along the south fence.");
        const unpublished = await session().fail("get_concept", {
            path,
            catalog: session().catalog,
        });
        expect(unpublished.error).toBe("NotFound");

        const pending = await session().call<{ files: { path: string; change: string }[] }>(
            "pending_changes",
            { changeset },
        );
        expect(pending.files).toContainEqual(expect.objectContaining({ path, change: "create" }));

        const published = await session().publish(changeset, "Add the garden plan");
        expect(published.published).toBe(true);
        expect((await session().concept(path)).body).toBe(draft.body);

        const history = await session().call<{ revisions: { op: string; message: string }[] }>(
            "history",
            { path, catalog: session().catalog },
        );
        expect(history.revisions).toEqual([
            expect.objectContaining({ op: "create", message: "Add the garden plan" }),
        ]);
    });
});

describe(SCENARIOS.addContextWithSources.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.addContextWithSources);
    const path = paths.at("history/old-footbridge");

    test("several sources on an existing concept keep their order; a duplicate id is refused", async () => {
        const changeset = await session().openChangeset();
        const sources: SourceEntry[] = [
            { id: "diary-1995-10-03", resource: "Family diary, 3 October 1995" },
            { id: "council-minutes-1995", resource: "Town council minutes, 1995" },
            { id: "plaque", resource: "Plaque on the bridge's east end" },
        ];
        for (const source of sources) {
            await session().call("add_source", { path, ...source, changeset });
        }
        const duplicate = await session().fail("add_source", {
            path,
            id: "plaque",
            resource: "the same plaque again",
            changeset,
        });
        expect(duplicate.error).toBe("Refused");

        await session().publish(changeset, "Source the footbridge note three ways");
        expect((await session().concept(path)).frontmatter.sources).toEqual(sources);
    });
});

describe(SCENARIOS.organizeIntoFolders.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.organizeIntoFolders);

    test("concepts created with summaries are listed in a new folder index", async () => {
        const concepts = [
            { name: "primary-sources", summary: "What counts as a primary source." },
            { name: "corroboration", summary: "Checking one source against another." },
            { name: "provenance", summary: "Where a document came from and who held it." },
        ];
        const changeset = await session().openChangeset();
        for (const { name, summary } of concepts) {
            await create(session(), changeset, {
                path: paths.at(`evidence/${name}`),
                title: name,
                description: summary,
                body: `# ${name}\n\n${summary}\n`,
                summary,
            });
        }
        await session().publish(changeset, "Add the evidence evaluation concepts");

        const folderIndex = await session().document(`${paths.folder}/evidence/index.md`);
        for (const { name, summary } of concepts) {
            expect(folderIndex).toContain(`(${name}.md) - ${summary}`);
        }
        const parentIndex = await session().document(`${paths.folder}/index.md`);
        expect(parentIndex).toContain("evidence/");
    });
    // Issue #1121: `index` on a concept the folder index already lists re-describes it,
    // and the edit replays at publish instead of conflicting with other changesets.
    const folder = "redescribe";
    const listing = (name: string, summary: string) => `(${name}.md) - ${summary}`;
    let seeded: Promise<void> | undefined;
    function seedListed(): Promise<void> {
        seeded ??= remember(session(), "List two concepts for re-description", [
            ...["alpha", "beta"].map((name) => ({
                path: paths.at(`${folder}/${name}`),
                title: name,
                body: `# ${name}\n\n${name} is listed.\n`,
                summary: `Original ${name} summary.`,
            })),
        ]);
        return seeded;
    }
    // `index` reads the published concept and index.md from the replica it reaches, so wait
    // until the listing is visible there before using it (README, "Reads after a publish").
    const visibleInIndex = (name: string) =>
        vi.waitFor(async () => {
            expect(await session().document(indexPath())).toContain(`(${name}.md)`);
        });
    const indexPath = () => `${paths.folder}/${folder}/index.md`;
    const redescribe = (changeset: string, name: string, summary: string) =>
        session().call<{ message: string; touched: string[] }>("index", {
            path: paths.at(`${folder}/${name}`),
            summary,
            changeset,
        });

    test("index with a new summary changes that listing's summary and nothing else", async () => {
        await seedListed();
        let before = "";
        await vi.waitFor(async () => {
            before = await session().document(indexPath());
            expect(before).toContain(listing("beta", "Original beta summary."));
        });
        const changeset = await session().openChangeset();
        await redescribe(changeset, "alpha", "Revised alpha summary.");

        const after = await session().document(indexPath(), changeset);
        expect(after).toBe(
            before.replace(
                listing("alpha", "Original alpha summary."),
                listing("alpha", "Revised alpha summary."),
            ),
        );
        expect(after).not.toBe(before);
        await session().publish(changeset, "Re-describe alpha");
        await vi.waitFor(async () => {
            expect(await session().document(indexPath())).toBe(after);
        });
    });

    test("index with the unchanged summary is a no-op", async () => {
        await remember(session(), "List delta", [
            {
                path: paths.at(`${folder}/delta`),
                title: "delta",
                body: "# delta\n\ndelta is listed.\n",
                summary: "Delta summary.",
            },
        ]);
        await visibleInIndex("delta");
        const changeset = await session().openChangeset();
        const report = await redescribe(changeset, "delta", "Delta summary.");

        expect(report.message).toContain("already lists");
        expect(
            (await session().call<{ files: unknown[] }>("pending_changes", { changeset })).files,
        ).toEqual([]);
        await session().call("discard", { changeset });
    });

    test("a re-describe still lands at publish after another changeset published the same index", async () => {
        await seedListed();
        await visibleInIndex("beta");
        const ours = await session().openChangeset();
        await redescribe(ours, "beta", "Replayed beta summary.");
        // Another changeset adds a listing to the same index.md and publishes first.
        const theirs = await session().openChangeset();
        await create(session(), theirs, {
            path: paths.at(`${folder}/gamma`),
            title: "gamma",
            body: "# gamma\n\ngamma is listed.\n",
            summary: "Gamma summary.",
        });
        await session().publish(theirs, "List gamma");
        const pending = await session().call<{ files: { path: string; stale: boolean; replayable: boolean }[] }>(
            "pending_changes",
            { changeset: ours },
        );
        expect(pending.files).toEqual([
            expect.objectContaining({ path: indexPath(), stale: true, replayable: true }),
        ]);

        const published = await session().publish(ours, "Re-describe beta");
        expect(published.published).toBe(true);
        await vi.waitFor(async () => {
            const text = await session().document(indexPath());
            expect(text).toContain(listing("beta", "Replayed beta summary."));
            expect(text).toContain(listing("gamma", "Gamma summary."));
        });
    });
});

describe(SCENARIOS.verifyAndStamp.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.verifyAndStamp);
    const path = paths.at("infra/ntp");

    test("review and verified stamp record who and when, with the server's clock", async () => {
        const before = Date.now();
        const changeset = await session().openChangeset();
        await session().call("review", {
            path,
            outcome: "verified",
            note: "Checked against my own hosts' sync logs.",
            by: "human:tester",
            changeset,
        });
        await session().call("stamp", { path, field: "verified", by: "human:tester", changeset });
        await session().publish(changeset, "Verify the NTP concept");
        const after = Date.now();

        const { frontmatter } = await session().concept(path);
        const reviewed = frontmatter.reviewed as Stamp;
        expect(reviewed).toMatchObject({
            by: "human:tester",
            outcome: "verified",
            note: "Checked against my own hosts' sync logs.",
        });
        const verified = frontmatter.verified as Stamp[];
        expect(verified).toEqual([expect.objectContaining({ by: "human:tester" })]);
        // Generous: the target's clock may differ from this machine's a little.
        for (const stamp of [reviewed, ...verified]) {
            const at = Date.parse(stamp.at);
            expect(at).toBeGreaterThan(before - 5 * 60_000);
            expect(at).toBeLessThan(after + 5 * 60_000);
        }
    });

    test("stamping a concept that does not exist is NotFound", async () => {
        const changeset = await session().openChangeset();
        const error = await session().fail("stamp", {
            path: paths.at("infra/no-such-system"),
            field: "verified",
            changeset,
        });
        expect(error.error).toBe("NotFound");
        await session().call("discard", { changeset });
    });
});
