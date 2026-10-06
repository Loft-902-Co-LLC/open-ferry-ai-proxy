import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, Pencil, Save, Trash2 } from "lucide-react";
import { useEffect, useId, useState } from "react";
import { useForm } from "react-hook-form";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import {
  DASHBOARD_API,
  USAGE_LEDGER,
  USAGE_PRICES,
  USAGE_RECORDS,
  type LedgerSettings,
  type LedgerState,
  type Price,
  type PriceInput,
  type Prices,
} from "../../api/dashboard";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ConfirmDialog } from "../../components/Dialog";
import { PageHeader } from "../../components/PageHeader";
import { ProblemNotice } from "../../components/ProblemNotice";
import { QueryState } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { Table, Td, Th } from "../../components/Table";
import { TextField } from "../../components/TextField";
import { decimalField, optionalDecimalField, wholeNumberField } from "../../lib/fields";
import { formatBytes, formatDateTime, formatInteger } from "../../lib/format";
import { z } from "../../lib/zod";
import { LedgerNotices } from "./LedgerNotices";

const USAGE_ROUTES = `${DASHBOARD_API}/usage/`;

// ------------------------------------------------------------ the ledger

function LedgerFacts({ ledger }: { ledger: LedgerState }) {
  const facts: [string, string][] = [
    ["File", ledger.file ?? "–"],
    ["Size", ledger.size_bytes === null ? "–" : formatBytes(ledger.size_bytes)],
    ["Calls recorded", ledger.rows === null ? "–" : formatInteger(ledger.rows)],
    ["Oldest", ledger.oldest === null ? "none" : formatDateTime(ledger.oldest)],
    ["Newest", ledger.newest === null ? "none" : formatDateTime(ledger.newest)],
    ["Recording", ledger.recording === true ? "Yes" : "No"],
  ];
  return (
    <dl className="grid gap-x-6 gap-y-2 sm:grid-cols-[max-content_1fr]">
      {facts.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="text-muted">{label}</dt>
          <dd className={label === "File" ? "font-mono break-all" : "tabular-nums"}>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

const settingsSchema = z.object({
  retention_days: wholeNumberField("Keep calls for", 1, 3650),
  max_rows: wholeNumberField("Keep at most", 10_000, 10_000_000),
  currency: z
    .string()
    .trim()
    .regex(/^[A-Za-z]{1,8}$/, "The currency is 1 to 8 letters, such as USD."),
});
type SettingsInput = z.input<typeof settingsSchema>;

function settingsValues(ledger: LedgerState): SettingsInput {
  return {
    retention_days: String(ledger.retention_days ?? ""),
    max_rows: String(ledger.max_rows ?? ""),
    currency: ledger.currency ?? "",
  };
}

function LedgerSettingsForm({ ledger }: { ledger: LedgerState }) {
  const call = useApiCall();
  const client = useQueryClient();
  const form = useForm<SettingsInput, unknown, z.output<typeof settingsSchema>>({
    resolver: zodResolver(settingsSchema),
    defaultValues: settingsValues(ledger),
  });
  const save = useMutation({
    mutationFn: (change: LedgerSettings) =>
      call<LedgerState>(USAGE_LEDGER, { method: "PATCH", json: change }),
    onSuccess: (state) => {
      client.setQueryData([USAGE_LEDGER], state);
      form.reset(settingsValues(state));
      // The currency shows beside every cost.
      void client.invalidateQueries({
        predicate: (query) =>
          typeof query.queryKey[0] === "string" &&
          query.queryKey[0].startsWith(USAGE_ROUTES) &&
          query.queryKey[0] !== USAGE_LEDGER,
      });
    },
  });

  const onSubmit = form.handleSubmit((values) => {
    const change: LedgerSettings = {};
    if (values.retention_days !== ledger.retention_days) {
      change.retention_days = values.retention_days;
    }
    if (values.max_rows !== ledger.max_rows) {
      change.max_rows = values.max_rows;
    }
    if (values.currency !== ledger.currency) {
      change.currency = values.currency;
    }
    if (Object.keys(change).length > 0) {
      save.mutate(change);
    }
  });
  const errors = form.formState.errors;

  return (
    <form noValidate onSubmit={(event) => void onSubmit(event)} className="space-y-4">
      <div className="grid gap-4 sm:grid-cols-3">
        <TextField
          label="Keep calls for (days)"
          inputMode="numeric"
          hint="1 to 3,650. Older calls are deleted."
          error={errors.retention_days?.message}
          {...form.register("retention_days")}
        />
        <TextField
          label="Keep at most (calls)"
          inputMode="numeric"
          hint="10,000 to 10,000,000. The oldest beyond it are deleted."
          error={errors.max_rows?.message}
          {...form.register("max_rows")}
        />
        <TextField
          label="Currency"
          autoCapitalize="characters"
          hint="Shown beside costs; nothing is converted."
          error={errors.currency?.message}
          {...form.register("currency")}
        />
      </div>
      <p className="text-muted">
        These are kept in the ledger file, not in config.yaml. Lowering a limit deletes calls
        within moments.
      </p>
      {save.isError && <ProblemNotice problem={callProblem(save.error)} live />}
      {save.isSuccess && !form.formState.isDirty && (
        <Alert tone="ok" live>
          <p>Saved.</p>
        </Alert>
      )}
      <Button type="submit" variant="primary" disabled={save.isPending || !form.formState.isDirty}>
        {save.isPending ? <Spinner /> : <Save aria-hidden="true" className="size-4" />}
        Save
      </Button>
    </form>
  );
}

// ------------------------------------------------------------ the prices

const priceSchema = z.object({
  model: z
    .string()
    .trim()
    .min(1, "Enter the model, as it is sent upstream.")
    .max(256, "A model is at most 256 characters."),
  input: decimalField("The input price", 0, 1_000_000),
  cache_read: optionalDecimalField("The cache read price", 0, 1_000_000),
  cache_write: optionalDecimalField("The cache write price", 0, 1_000_000),
  output: decimalField("The output price", 0, 1_000_000),
});
type PriceForm = z.input<typeof priceSchema>;

const EMPTY_PRICE: PriceForm = { model: "", input: "", cache_read: "", cache_write: "", output: "" };

function priceValues(price: Price): PriceForm {
  const text = (value: number | null) => (value === null ? "" : String(value));
  return {
    model: price.model,
    input: text(price.input),
    cache_read: text(price.cache_read),
    cache_write: text(price.cache_write),
    output: text(price.output),
  };
}

function invalidateCosts(client: ReturnType<typeof useQueryClient>) {
  return client.invalidateQueries({
    predicate: (query) =>
      typeof query.queryKey[0] === "string" && query.queryKey[0].startsWith(USAGE_ROUTES),
  });
}

function PriceEditor({
  prices,
  editing,
  onSaved,
}: {
  prices: Prices;
  /** The values to edit: a price, or a model to price. */
  editing: PriceForm;
  onSaved: () => void;
}) {
  const call = useApiCall();
  const client = useQueryClient();
  const listId = useId();
  const form = useForm<PriceForm, unknown, z.output<typeof priceSchema>>({
    resolver: zodResolver(priceSchema),
    defaultValues: editing,
  });
  useEffect(() => {
    form.reset(editing);
  }, [editing, form]);
  const save = useMutation({
    mutationFn: (price: PriceInput) => call<Price>(USAGE_PRICES, { method: "PUT", json: price }),
    onSuccess: async () => {
      form.reset(EMPTY_PRICE);
      onSaved();
      await invalidateCosts(client);
    },
  });
  const onSubmit = form.handleSubmit((values) => {
    save.mutate(values);
  });
  const errors = form.formState.errors;
  const unit = `${prices.currency} per million tokens`;

  return (
    <form
      noValidate
      aria-label="Set a price"
      onSubmit={(event) => void onSubmit(event)}
      className="space-y-4"
    >
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-5">
        <TextField
          label="Model"
          list={listId}
          spellCheck={false}
          autoCapitalize="none"
          className="sm:col-span-2 lg:col-span-1"
          error={errors.model?.message}
          {...form.register("model")}
        />
        <datalist id={listId}>
          {prices.unpriced_models.map((model) => (
            <option key={model} value={model} />
          ))}
        </datalist>
        <TextField
          label="Input"
          inputMode="decimal"
          hint={unit}
          error={errors.input?.message}
          {...form.register("input")}
        />
        <TextField
          label="Cache read"
          inputMode="decimal"
          hint="Empty: as input"
          error={errors.cache_read?.message}
          {...form.register("cache_read")}
        />
        <TextField
          label="Cache write"
          inputMode="decimal"
          hint="Empty: as input"
          error={errors.cache_write?.message}
          {...form.register("cache_write")}
        />
        <TextField
          label="Output"
          inputMode="decimal"
          hint={unit}
          error={errors.output?.message}
          {...form.register("output")}
        />
      </div>
      {save.isError && <ProblemNotice problem={callProblem(save.error)} live />}
      {save.isSuccess && (
        <Alert tone="ok" live>
          <p>Price saved. Costs, past ones too, now use it.</p>
        </Alert>
      )}
      <div className="flex flex-wrap gap-2">
        <Button type="submit" variant="primary" disabled={save.isPending}>
          {save.isPending ? <Spinner /> : <Save aria-hidden="true" className="size-4" />}
          Save price
        </Button>
        {form.formState.isDirty && (
          <Button
            onClick={() => {
              form.reset(EMPTY_PRICE);
            }}
          >
            Clear
          </Button>
        )}
      </div>
    </form>
  );
}

function PriceTable({
  prices,
  onEdit,
}: {
  prices: Prices;
  onEdit: (price: Price) => void;
}) {
  const call = useApiCall();
  const client = useQueryClient();
  const remove = useMutation({
    mutationFn: (model: string) =>
      call<{ deleted: boolean }>(USAGE_PRICES, { method: "DELETE", query: { model } }),
    onSuccess: () => invalidateCosts(client),
  });
  const price = (value: number | null) => (value === null ? "as input" : String(value));

  if (prices.prices.length === 0) {
    return <p className="text-muted">No prices yet, so no costs are shown.</p>;
  }
  return (
    <>
      <Table caption={`Prices, in ${prices.currency} per million tokens`}>
        <thead>
          <tr>
            <Th>Model</Th>
            <Th className="text-right">Input</Th>
            <Th className="text-right">Cache read</Th>
            <Th className="text-right">Cache write</Th>
            <Th className="text-right">Output</Th>
            <Th>Changed</Th>
            <Th>
              <span className="sr-only">Actions</span>
            </Th>
          </tr>
        </thead>
        <tbody>
          {prices.prices.map((entry) => (
            <tr key={entry.model}>
              <Td className="font-mono">{entry.model}</Td>
              <Td className="text-right">{String(entry.input)}</Td>
              <Td className="text-right">{price(entry.cache_read)}</Td>
              <Td className="text-right">{price(entry.cache_write)}</Td>
              <Td className="text-right">{String(entry.output)}</Td>
              <Td className="whitespace-nowrap">{formatDateTime(entry.updated)}</Td>
              <Td className="text-right whitespace-nowrap">
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label={`Change the price of ${entry.model}`}
                  onClick={() => {
                    onEdit(entry);
                  }}
                >
                  <Pencil aria-hidden="true" className="size-4" />
                  Change
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label={`Remove the price of ${entry.model}`}
                  disabled={remove.isPending && remove.variables === entry.model}
                  onClick={() => {
                    remove.mutate(entry.model);
                  }}
                >
                  <Trash2 aria-hidden="true" className="size-4" />
                  Remove
                </Button>
              </Td>
            </tr>
          ))}
        </tbody>
      </Table>
      {remove.isError && <ProblemNotice problem={callProblem(remove.error)} live />}
    </>
  );
}

function PricesCard() {
  const prices = useApiQuery<Prices>(USAGE_PRICES);
  const [editing, setEditing] = useState<PriceForm>(EMPTY_PRICE);
  return (
    <Card
      title={<span id="prices">Prices</span>}
      description="Per million tokens. A cost is worked out when shown, so a price applies to past calls too."
    >
      <QueryState query={prices} loading="Loading prices…">
        {(data) => (
          <>
            <PriceTable
              prices={data}
              onEdit={(price) => {
                setEditing(priceValues(price));
              }}
            />
            {data.unpriced_models.length > 0 && (
              <div className="space-y-2">
                <p>Models with calls and no price:</p>
                <ul className="flex flex-wrap gap-2">
                  {data.unpriced_models.map((model) => (
                    <li key={model}>
                      <Button
                        size="sm"
                        aria-label={`Set a price for ${model}`}
                        onClick={() => {
                          setEditing({ ...EMPTY_PRICE, model });
                        }}
                      >
                        <span className="font-mono">{model}</span>
                      </Button>
                    </li>
                  ))}
                </ul>
              </div>
            )}
            <PriceEditor
              prices={data}
              editing={editing}
              onSaved={() => {
                setEditing(EMPTY_PRICE);
              }}
            />
          </>
        )}
      </QueryState>
    </Card>
  );
}

// ----------------------------------------------------- deleting the calls

function DeleteRecords({ ledger }: { ledger: LedgerState }) {
  const call = useApiCall();
  const client = useQueryClient();
  const [asking, setAsking] = useState(false);
  const remove = useMutation({
    mutationFn: () => call<{ deleted: number }>(USAGE_RECORDS, { method: "DELETE" }),
    onSuccess: async () => {
      setAsking(false);
      await invalidateCosts(client);
    },
  });
  const rows = ledger.rows ?? 0;
  return (
    <Card
      title="Delete the recorded calls"
      description="Deletes every call in the ledger. Its settings and the prices stay."
    >
      {remove.isSuccess && (
        <Alert tone="ok" live>
          <p>Deleted {formatInteger(remove.data.deleted)} calls.</p>
        </Alert>
      )}
      {remove.isError && !asking && <ProblemNotice problem={callProblem(remove.error)} live />}
      <Button
        variant="danger"
        disabled={rows === 0}
        onClick={() => {
          remove.reset();
          setAsking(true);
        }}
      >
        <Trash2 aria-hidden="true" className="size-4" />
        Delete all recorded calls
      </Button>
      <ConfirmDialog
        open={asking}
        title="Delete every recorded call?"
        confirmLabel={`Delete ${formatInteger(rows)} calls`}
        pending={remove.isPending}
        onConfirm={() => {
          remove.mutate();
        }}
        onCancel={() => {
          setAsking(false);
        }}
      >
        <p>
          All {formatInteger(rows)} calls in the ledger are deleted, and usage starts again from
          nothing. This can&apos;t be undone.
        </p>
        {remove.isError && <ProblemNotice problem={callProblem(remove.error)} live />}
      </ConfirmDialog>
    </Card>
  );
}

export function LedgerPage() {
  const ledger = useApiQuery<LedgerState>(USAGE_LEDGER);
  return (
    <>
      <PageHeader
        title="Ledger and prices"
        description="Where usage is kept, for how long, and what calls cost."
        actions={
          <Link to="/usage">
            <ArrowLeft aria-hidden="true" className="mr-1 inline size-4" />
            Back to usage
          </Link>
        }
      />
      <QueryState query={ledger} loading="Loading the usage ledger…">
        {(state) => (
          <div className="space-y-4">
            <LedgerNotices ledger={state} />
            {state.available && (
              <>
                <Card title="The ledger">
                  <LedgerFacts ledger={state} />
                  <p className="text-muted">
                    Calls are recorded while <Code>usage-statistics-enabled</Code> is on. No prompt
                    or answer text is kept, and no client key in clear.
                  </p>
                </Card>
                <Card title="Keeping">
                  <LedgerSettingsForm ledger={state} />
                </Card>
                <PricesCard />
                <DeleteRecords ledger={state} />
              </>
            )}
          </div>
        )}
      </QueryState>
    </>
  );
}
