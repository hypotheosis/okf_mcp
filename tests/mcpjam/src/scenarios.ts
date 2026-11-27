// The swarm goals this suite replays, and the fixture each one starts from.
//
// Every scenario gets its own folder, `/<runFolder>/<slug>/`, with one seed
// concept in it, published by the global setup in a single changeset before any
// test runs. So each scenario starts with its folder index in place (no
// first-write race on a shared index), and goals that assume an existing
// concept, like "Verify and stamp a fact", find it.

/** One seed concept, relative to its scenario's folder. */
export interface Seed {
    /** Path under the scenario folder, without `.md`, e.g. `infra/ntp`. */
    path: string;
    type: string;
    title: string;
    description: string;
    body: string;
}

/** A scenario: the MCPJam goal it replays and what it is seeded with. */
export interface Scenario {
    /** The MCPJam goal's name, verbatim, for tracing a red test back to the swarm. */
    goal: string;
    /** Its folder name under the run folder. */
    slug: string;
    seeds: Seed[];
}

/** A seed for scenarios that need nothing but their folder. */
function folderSeed(slug: string): Seed {
    return {
        path: "readme",
        type: "Note",
        title: `Scenario ${slug}`,
        description: `Seed concept for the ${slug} scenario.`,
        body: `# Scenario ${slug}\n\nThe scenario's folder exists from the start.\n`,
    };
}

function scenario(goal: string, slug: string, extra: Seed[] = []): Scenario {
    return { goal, slug, seeds: [folderSeed(slug), ...extra] };
}

const NTP_SEED: Seed = {
    path: "infra/ntp",
    type: "System",
    title: "NTP",
    description: "The network time server the hosts sync to.",
    body: "# NTP\n\nThe hosts sync their clocks to the network time server.\n",
};

const FOOTBRIDGE_SEED: Seed = {
    path: "history/old-footbridge",
    type: "Note",
    title: "Old footbridge",
    description: "The old footbridge was rebuilt in 1995.",
    body: "# Old footbridge\n\nThe old footbridge was rebuilt in 1995.\n",
};

/** Every scenario, keyed by slug. */
export const SCENARIOS = {
    // Careful Fact Curator
    createAndSource: scenario("Create and source a fact", "create-and-source"),
    reviewAndPublish: scenario("Review and publish a draft", "review-and-publish"),
    addContextWithSources: scenario("Add context with sources", "add-context-with-sources", [
        FOOTBRIDGE_SEED,
    ]),
    organizeIntoFolders: scenario("Organize facts into folders", "organize-into-folders"),
    verifyAndStamp: scenario("Verify and stamp a fact", "verify-and-stamp", [NTP_SEED]),
    // Skeptical Recall Prober
    createAndRetrieve: scenario("Create and retrieve a stored fact", "create-and-retrieve"),
    scatteredFacts: scenario("Feed scattered facts and audit retention", "scattered-facts"),
    sourceAttribution: scenario("Test source attribution accuracy", "source-attribution"),
    editConflicts: scenario("Provoke and inspect edit conflicts", "edit-conflicts"),
    hallucinationAudit: scenario(
        "Catch hallucinated recall with a search audit",
        "hallucination-audit",
    ),
    // Playful Memory Tester
    quiz: scenario("Quiz the assistant on facts", "quiz"),
    knowledgeBase: scenario("Build a personal facts knowledge base", "knowledge-base"),
    surpriseQuestions: scenario("Test memory with surprise questions", "surprise-questions"),
    indexedArchive: scenario("Create indexed memory archive", "indexed-archive"),
    captureInterview: scenario("Quick memory capture interview", "capture-interview"),
    // Not a swarm goal: what the server's tool list tells a client without a system prompt.
    toolDescriptions: scenario("Tool descriptions steer a client to remember", "tool-descriptions"),
    // Not a swarm goal: the shared-catalog races the swarm runs exposed.
    concurrency: scenario("Concurrent sessions on one catalog", "concurrency", [
        {
            path: "shared/readme",
            type: "Note",
            title: "Shared folder",
            description: "An existing folder several sessions write into at once.",
            body: "# Shared folder\n\nThe line every session edits.\n",
        },
    ]),
} satisfies Record<string, Scenario>;

/** The absolute folder of a scenario in this run, e.g. `/swarm-x/quiz`. */
export function folderOf(runFolder: string, scenario: Scenario): string {
    return `/${runFolder}/${scenario.slug}`;
}
