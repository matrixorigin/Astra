'use client';

import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Modal } from '@/components/ui/modal';
import { Textarea } from '@/components/ui/textarea';

export function InstructionsModal({
  open,
  initialValue,
  onOpenChange,
  onSave,
}: {
  open: boolean;
  initialValue: string;
  onOpenChange: (open: boolean) => void;
  onSave: (value: string) => Promise<void>;
}) {
  const [value, setValue] = useState(initialValue);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (open) {
      setValue(initialValue);
    }
  }, [initialValue, open]);

  function close() {
    if (value !== initialValue && !window.confirm('Discard?')) {
      return;
    }
    onOpenChange(false);
  }

  return (
    <Modal open={open} onOpenChange={(next) => (next ? onOpenChange(next) : close())} title="Project instructions" width={720}>
      <div className="space-y-4 p-5">
        <Textarea
          value={value}
          maxLength={8000}
          onChange={(event) => setValue(event.target.value)}
          className="min-h-72"
          placeholder="Add instructions the agent should follow in this project..."
        />
        <div className="flex items-center justify-between">
          <span className="text-xs text-text-muted">{value.length}/8000</span>
          <div className="flex gap-3">
            <Button type="button" variant="ghost" onClick={close}>Cancel</Button>
            <Button
              type="button"
              variant="primary"
              disabled={saving}
              onClick={async () => {
                setSaving(true);
                try {
                  await onSave(value);
                  onOpenChange(false);
                } finally {
                  setSaving(false);
                }
              }}
            >
              {saving ? 'Saving...' : 'Save'}
            </Button>
          </div>
        </div>
      </div>
    </Modal>
  );
}
