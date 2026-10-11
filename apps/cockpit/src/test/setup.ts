// React Testing Library needs the act() environment flag outside of react-dom/test-utils.
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
