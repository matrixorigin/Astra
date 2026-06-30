export type SelectedModelInput = {
  id?: string;
  model: string;
  gateway?: string;
};

export type SelectedModelWire = {
  id?: string;
  model: string;
  gateway?: string;
};

export function selectedModelToWire(
  selectedModel: SelectedModelInput | undefined,
): SelectedModelWire {
  if (
    !selectedModel ||
    typeof selectedModel.model !== "string" ||
    selectedModel.model.length === 0
  ) {
    throw new Error("selectedModel.model is required");
  }
  const wire: SelectedModelWire = {
    model: selectedModel.model,
  };
  if (selectedModel.id !== undefined) {
    if (typeof selectedModel.id !== "string" || selectedModel.id.length === 0) {
      throw new Error(
        "selectedModel.id must be a non-empty string when provided",
      );
    }
    wire.id = selectedModel.id;
  }
  if (selectedModel.gateway !== undefined) {
    if (typeof selectedModel.gateway !== "string") {
      throw new Error("selectedModel.gateway must be a string when provided");
    }
    wire.gateway = selectedModel.gateway;
  }
  return wire;
}
