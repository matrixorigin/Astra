export type ModelSelectionInput = {
  offeringId: string;
};

export type ModelSelectionWire = {
  offering_id: string;
};

export function modelSelectionToWire(
  selection: ModelSelectionInput | undefined,
): ModelSelectionWire {
  if (
    !selection ||
    typeof selection.offeringId !== "string" ||
    selection.offeringId.length === 0
  ) {
    throw new Error("modelSelection.offeringId is required");
  }
  const unknown = Object.keys(selection).filter((key) => key !== "offeringId");
  if (unknown.length > 0) {
    throw new Error(`modelSelection contains unsupported field '${unknown[0]}'`);
  }
  if (
    selection.offeringId.trim() !== selection.offeringId ||
    /[\u0000-\u001f\u007f]/u.test(selection.offeringId)
  ) {
    throw new Error("modelSelection.offeringId must be an exact identifier");
  }
  if (new TextEncoder().encode(selection.offeringId).length > 64) {
    throw new Error("modelSelection.offeringId must be at most 64 bytes");
  }
  return { offering_id: selection.offeringId };
}
