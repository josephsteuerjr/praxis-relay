const supportedLevels = new Set([
  "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
]);

export function reasoningChoices(models, selectedModel) {
  const entry = selectedModel
    ? models.find((model) => model.id === selectedModel)
    : models[0];
  const advertised = Array.isArray(entry?.reasoning_efforts)
    ? entry.reasoning_efforts
    : [];
  return [
    "default",
    ...new Set(advertised.filter((level) => supportedLevels.has(level))),
  ];
}
