(function (root, factory) {
  const workflows = factory();
  if (typeof module === "object" && module.exports) module.exports = workflows;
  root.TinyMemoryWorkflows = workflows;
})(typeof globalThis === "object" ? globalThis : this, function () {
  function uploadRequest(file, fields, formDataFactory) {
    const body = formDataFactory();
    body.append("namespace", fields.namespace || "documents");
    body.append("key", file.name);
    body.append("category", fields.category);
    body.append("taint", fields.taint);
    body.append("file", file, file.name);
    return { path: "/documents/upload", options: { method: "POST", body } };
  }

  function wireUpload(config) {
    config.button.addEventListener("click", () => config.run(async () => {
      const files = Array.from(config.filesInput.files || []);
      if (files.length === 0) throw new Error("choose at least one file first");
      const fields = {
        namespace: config.namespaceInput.value,
        category: config.categoryInput.value,
        taint: config.taintInput.value,
      };
      const results = [];
      for (const file of files) {
        try {
          const request = uploadRequest(file, fields, () => new FormData());
          await config.call(request.path, request.options);
          results.push({ file: file.name, bytes: file.size, status: "stored" });
        } catch (error) {
          results.push({ file: file.name, bytes: file.size, status: "error: " + error.message });
        }
      }
      return results;
    }));
  }

  return { uploadRequest, wireUpload };
});
