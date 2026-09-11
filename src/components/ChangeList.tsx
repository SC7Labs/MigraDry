import type { SchemaChange } from "../types/migration";
import {
  changeLabel,
  changeNote,
  changeSymbol,
  describeDataImpact,
  describePropertyChange,
  isAddition,
} from "../lib/format";

interface ChangeListProps {
  changes: SchemaChange[];
  heading: string;
  emptyMessage: string;
}

function symbolLabel(change: SchemaChange): string {
  if (
    change.kind === "column_modified" ||
    change.kind === "table_definition_changed"
  ) {
    return "modified";
  }
  return isAddition(change.kind) ? "added" : "removed";
}

/**
 * A definition change says only that the `CREATE TABLE` text moved. MigraDry
 * does not parse SQL, so it does not claim to know how — most often it is the
 * expression inside a generated column, which no column-level metadata reports.
 */
const DEFINITION_NOTE =
  "The CREATE TABLE text changed in a way column metadata does not show — " +
  "most often the expression inside a generated column. MigraDry reports that " +
  "it moved, not what it now means.";

/** The heart of the report: what the migration would add, remove and alter. */
export function ChangeList({ changes, heading, emptyMessage }: ChangeListProps) {
  return (
    <section className="changes-section">
      <h3 className="changes-section__heading">{heading}</h3>
      {changes.length === 0 ? (
        <p className="muted">{emptyMessage}</p>
      ) : (
        <ul className="changes">
          {changes.map((change) => {
            const note = changeNote(change);
            return (
              <li
                key={`${change.kind}:${change.parentName ?? ""}:${change.objectName}`}
                className={`changes__item changes__item--${change.impact}`}
              >
                <div className="changes__headline">
                  <span
                    className="changes__symbol"
                    aria-label={symbolLabel(change)}
                  >
                    {changeSymbol(change.kind)}
                  </span>
                  <span className="changes__label">{changeLabel(change)}</span>
                  {note && (
                    <span className="changes__note">
                      {change.impact === "destructive" && (
                        <span aria-hidden="true">! </span>
                      )}
                      {note}
                    </span>
                  )}
                </div>

                {change.propertyChanges.length > 0 && (
                  <dl className="changes__properties">
                    {change.propertyChanges.map((property) => {
                      const { label, before, after } =
                        describePropertyChange(property);
                      return (
                        <div className="changes__property" key={property.property}>
                          <dt>{label}</dt>
                          <dd>
                            <span className="changes__before">{before}</span>
                            <span className="changes__arrow" aria-label="becomes">
                              {" → "}
                            </span>
                            <span className="changes__after">{after}</span>
                          </dd>
                        </div>
                      );
                    })}
                  </dl>
                )}

                {change.kind === "table_definition_changed" && (
                  <p className="changes__definition-note">{DEFINITION_NOTE}</p>
                )}

                {change.dataImpact && (
                  <p
                    className={`changes__impact${
                      change.dataImpact.status === "unavailable"
                        ? " changes__impact--unavailable"
                        : ""
                    }`}
                  >
                    {describeDataImpact(change.dataImpact)}
                  </p>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
