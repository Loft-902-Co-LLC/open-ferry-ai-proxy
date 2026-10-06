import { Link } from "react-router";

import { PageHeader } from "../components/PageHeader";

export function NotFoundPage() {
  return (
    <>
      <PageHeader title="Page not found" />
      <p>
        The dashboard has no page at this address. <Link to="/">Go to the overview</Link>.
      </p>
    </>
  );
}
