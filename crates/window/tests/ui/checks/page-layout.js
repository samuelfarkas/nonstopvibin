// The built app with one running profile, framed at each size: page geometry is identical
// across pages and widths. The frame is the viewport the window would be, in browser mode
// as the Electron check ran it, and free of the window's minimum size.
run(async () => {
  const frame = Object.assign(document.createElement("iframe"), {
    src: `/?layout#session=${config.token}`,
  });
  frame.style.cssText = "border: 0; display: block";
  document.body.append(frame);
  const inner = (selector) => frame.contentDocument?.querySelector(selector);
  await until(
    () => inner(".main-nav"),
    "the app renders its navigation",
    30000,
  );
  // Each page and a block only its rendered content has; geometry is the page body's first block.
  const pages = [
    ["Subscriptions", ".accounts"],
    ["Activity", ".activity-overview"],
    ["Connect agents", ".connect-page .agent-tabs"],
    ["Settings", "#usage-retention"],
  ];
  for (const width of [360, 620, 850, 1280, 1800, 2400]) {
    const samples = [];
    // Short enough that every page scrolls, and tall enough that none does.
    for (const height of [200, 3000]) {
      Object.assign(frame.style, {
        width: `${width}px`,
        height: `${height}px`,
      });
      const view = () => frame.contentWindow;
      await until(
        () => view().innerWidth === width && view().innerHeight === height,
        `the frame is ${width}x${height}`,
      );
      for (const [title, ready] of pages) {
        inner(`[title=${JSON.stringify(title)}]`).click();
        await until(() => inner(`.page-body ${ready}`), `${title} renders`);
        const rect = (selector) => {
          const { x, y, width, height } =
            inner(selector).getBoundingClientRect();
          return { x, y, width, height };
        };
        const page = frame.contentDocument.documentElement;
        samples.push({
          page: title,
          height,
          header: rect(".page-header"),
          title: rect(".page-title"),
          content: rect(".page-body > :first-child"),
          scrolls: page.scrollHeight > page.clientHeight,
          overflow: page.scrollWidth > page.clientWidth,
        });
      }
    }
    const baseline = samples[0];
    for (const sample of samples) {
      const context = `${width}px ${sample.page} at ${sample.height}px`;
      equal(
        sample.scrolls,
        sample.height === 200,
        `${context}: vertical overflow is not what the check needs`,
      );
      equal(sample.header, baseline.header, `${context}: header moved`);
      equal(
        sample.title.x,
        baseline.title.x,
        `${context}: title shifted sideways`,
      );
      equal(
        sample.title.y,
        baseline.title.y,
        `${context}: title shifted vertically`,
      );
      equal(
        sample.content.x,
        baseline.content.x,
        `${context}: content shifted`,
      );
      equal(
        sample.content.y,
        baseline.content.y,
        `${context}: content shifted vertically`,
      );
      equal(
        sample.content.width,
        baseline.content.width,
        `${context}: content resized`,
      );
      equal(
        sample.content.x,
        sample.title.x,
        `${context}: content/header misaligned`,
      );
      equal(sample.overflow, false, `${context}: horizontal overflow`);
    }
  }
});
