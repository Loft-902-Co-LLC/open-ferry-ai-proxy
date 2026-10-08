import { callProblem } from "../api/access";
import { useServerBuild } from "../api/serverBuild";
import { Card } from "../components/Card";
import { PageHeader } from "../components/PageHeader";
import { ProblemNotice } from "../components/ProblemNotice";
import { Spinner } from "../components/Spinner";
import { UpdatesCard } from "./about/UpdatesCard";

const LICENSES_URL = `${import.meta.env.BASE_URL}third-party-licenses.txt`;
const REPO_URL = "https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy";
const DOCS_URL = `${REPO_URL}/tree/main/docs`;
const DASHBOARD_API_URL = `${REPO_URL}/blob/main/docs/dashboard-api.md`;

function Fact({ term, value }: { term: string; value: string | null }) {
  return (
    <div className="flex flex-wrap gap-x-3">
      <dt className="w-28 shrink-0 text-muted">{term}</dt>
      <dd className="min-w-0 font-mono break-all">{value ?? "not reported"}</dd>
    </div>
  );
}

export function AboutPage() {
  const build = useServerBuild();
  return (
    <>
      <PageHeader title="About" description="What this is, where it comes from, its licenses, and its updates." />
      <div className="grid gap-4 lg:grid-cols-2">
        <Card title="open-ferry">
          <p>
            open-ferry is a Rust port of{" "}
            <a href="https://github.com/router-for-me/CLIProxyAPI" rel="noreferrer" target="_blank">
              CLIProxyAPI
            </a>
            , the MIT-licensed proxy by router-for-me. It keeps CLIProxyAPI&apos;s configuration
            and management API, and adds this dashboard.
          </p>
          <p>
            How to install, set up and run it is in{" "}
            <a href={REPO_URL} rel="noreferrer" target="_blank">
              open-ferry&apos;s README
            </a>
            , and the rest is in{" "}
            <a href={DOCS_URL} rel="noreferrer" target="_blank">
              its docs
            </a>
            , on GitHub. The API this dashboard uses is described in{" "}
            <a href={DASHBOARD_API_URL} rel="noreferrer" target="_blank">
              dashboard-api.md
            </a>
            .
          </p>
          <p>
            The dashboard is built from open-source packages. Their licenses and notices are in{" "}
            <a href={LICENSES_URL}>third-party-licenses.txt</a>.
          </p>
        </Card>
        <Card title="This server">
          {build.isPending && (
            <p className="flex items-center gap-2 text-muted">
              <Spinner /> Asking the server…
            </p>
          )}
          {build.isError && <ProblemNotice problem={callProblem(build.error)} />}
          {build.isSuccess && (
            <dl className="space-y-1.5">
              <Fact term="Version" value={build.data.version} />
              <Fact term="Commit" value={build.data.commit} />
              <Fact term="Built" value={build.data.buildDate} />
            </dl>
          )}
        </Card>
        <UpdatesCard className="lg:col-span-2" />
      </div>
    </>
  );
}
