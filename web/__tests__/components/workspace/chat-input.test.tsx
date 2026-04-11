import { render, screen, fireEvent } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ChatInput } from '@/components/workspace/chat-input';

describe('ChatInput', () => {
  const defaultProps = {
    onSend: jest.fn(),
    onStop: jest.fn(),
  };

  beforeEach(() => {
    jest.clearAllMocks();
  });

  it('renders textarea and Send button by default', () => {
    render(<ChatInput {...defaultProps} />);
    expect(screen.getByPlaceholderText('Send a message…')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Send' })).toBeInTheDocument();
  });

  it('renders Stop button when streaming', () => {
    render(<ChatInput {...defaultProps} isStreaming />);
    expect(screen.getByRole('button', { name: 'Stop' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Send' })).not.toBeInTheDocument();
  });

  it('renders Send button when not streaming', () => {
    render(<ChatInput {...defaultProps} isStreaming={false} />);
    expect(screen.getByRole('button', { name: 'Send' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Stop' })).not.toBeInTheDocument();
  });

  it('calls onSend with trimmed text when clicking Send', async () => {
    const user = userEvent.setup();
    render(<ChatInput {...defaultProps} />);
    const textarea = screen.getByPlaceholderText('Send a message…');
    await user.type(textarea, '  Hello world  ');
    await user.click(screen.getByRole('button', { name: 'Send' }));
    expect(defaultProps.onSend).toHaveBeenCalledWith('Hello world');
  });

  it('clears textarea after sending', async () => {
    const user = userEvent.setup();
    render(<ChatInput {...defaultProps} />);
    const textarea = screen.getByPlaceholderText('Send a message…') as HTMLTextAreaElement;
    await user.type(textarea, 'Hello');
    await user.click(screen.getByRole('button', { name: 'Send' }));
    expect(textarea.value).toBe('');
  });

  it('calls onStop when clicking Stop', async () => {
    const user = userEvent.setup();
    render(<ChatInput {...defaultProps} isStreaming />);
    await user.click(screen.getByRole('button', { name: 'Stop' }));
    expect(defaultProps.onStop).toHaveBeenCalled();
  });

  it('sends on Enter key press', () => {
    render(<ChatInput {...defaultProps} />);
    const textarea = screen.getByPlaceholderText('Send a message…');
    fireEvent.change(textarea, { target: { value: 'Hello' } });
    fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: false });
    expect(defaultProps.onSend).toHaveBeenCalledWith('Hello');
  });

  it('does not send on Shift+Enter', () => {
    render(<ChatInput {...defaultProps} />);
    const textarea = screen.getByPlaceholderText('Send a message…');
    fireEvent.change(textarea, { target: { value: 'Hello' } });
    fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: true });
    expect(defaultProps.onSend).not.toHaveBeenCalled();
  });

  it('Send button is disabled when textarea is empty', () => {
    render(<ChatInput {...defaultProps} />);
    const button = screen.getByRole('button', { name: 'Send' });
    expect(button).toBeDisabled();
  });

  it('Send button is disabled when only whitespace', async () => {
    const user = userEvent.setup();
    render(<ChatInput {...defaultProps} />);
    const textarea = screen.getByPlaceholderText('Send a message…');
    await user.type(textarea, '   ');
    expect(screen.getByRole('button', { name: 'Send' })).toBeDisabled();
  });

  it('does not call onSend when disabled', () => {
    render(<ChatInput {...defaultProps} disabled />);
    const textarea = screen.getByPlaceholderText('Send a message…');
    fireEvent.change(textarea, { target: { value: 'Hello' } });
    fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: false });
    expect(defaultProps.onSend).not.toHaveBeenCalled();
  });

  it('uses custom placeholder', () => {
    render(<ChatInput {...defaultProps} placeholder="Ask me anything…" />);
    expect(screen.getByPlaceholderText('Ask me anything…')).toBeInTheDocument();
  });

  it('shows a followup suggestion chip when the input is empty', () => {
    render(<ChatInput {...defaultProps} followupSuggestion="run the tests" />);
    expect(screen.getByRole('button', { name: 'Next: run the tests' })).toBeInTheDocument();
  });

  it('accepts the followup suggestion on Tab', () => {
    render(<ChatInput {...defaultProps} followupSuggestion="run the tests" />);
    const textarea = screen.getByPlaceholderText('Send a message…') as HTMLTextAreaElement;
    fireEvent.keyDown(textarea, { key: 'Tab' });
    expect(textarea.value).toBe('run the tests');
  });
});
